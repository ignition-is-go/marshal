//! Claude Code Monitor and asyncRewake delivery.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use marshal_client::config::config_file_candidates;
use marshal_client::hook_http::PostFailure;
use marshal_client::hooks::{
    agent_id_from_env, http_post_why, operator, registration_body, resolve_base, short_host, url_q,
};

/// `marshal-stdio watch <session_id> [base-url]` — the live inbox for Claude
/// Code without the channels flag. Run under Claude's Monitor tool: it
/// long-polls the daemon's `/hook/wait` and prints each `<marshal_inbox>` block
/// it gets, and every line it prints becomes a Monitor event, which wakes an
/// idle session. Runs until killed (the Monitor's expiry); a daemon that can't
/// be reached is retried, never fatal.
///
/// With `once` it exits after the first delivery instead, for running as a
/// background Bash command: that has no 30-minute limit and costs no model
/// turns while idle, and its exit is what wakes the session. Nothing bounds its
/// lifetime, so it also exits between polls once its parent is gone, rather
/// than taking mail for a session that has closed.
pub fn watch(session_id: &str, base_override: Option<&str>, once: bool) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    wait_for_mail(
        session_id,
        &cwd,
        base_override,
        once,
        parent_pid(),
        |resp| write_watch_events(&mut std::io::stdout().lock(), resp),
    );
}

/// `marshal-stdio claude-hook wait` — the flag-free wake for Claude Code, run as
/// an `asyncRewake` hook on SessionStart and Stop. It holds a `/hook/wait`
/// long-poll and, when mail arrives, prints it to stderr and exits 2, which
/// starts a turn even in a session nobody has typed to. Stop runs it again, so
/// no agent has to keep anything armed. Stop fires every turn, so a per-session
/// lock keeps it to one waiter; the lock is released whenever the process ends.
pub fn wait_hook(base_override: Option<&str>) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);
    let sid = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    if sid.is_empty() {
        return;
    }
    let cwd = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    let parent = parent_pid();
    let deadline = Instant::now() + lock_wait();
    let Some(_lock) = acquire_waiter_lock(sid, deadline, parent) else {
        return;
    };
    let delivered = wait_for_mail(sid, cwd, base_override, true, parent, |resp| {
        let mut out = std::io::stderr().lock();
        writeln!(out, "{resp}").and_then(|()| out.flush())
    });
    if delivered {
        std::process::exit(2);
    }
}

fn acquire_waiter_lock(
    session_id: &str,
    deadline: Instant,
    parent: Option<u32>,
) -> Option<std::fs::File> {
    loop {
        if parent_pid() != parent {
            return None;
        }
        if let Some(lock) = waiter_lock(session_id) {
            return Some(lock);
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        std::thread::sleep(LOCK_RETRY.min(remaining));
    }
}

/// Take this session's waiter lock, or `None` if another waiter holds it.
fn waiter_lock(session_id: &str) -> Option<std::fs::File> {
    let name = format!("waiter-{session_id}");
    let path = config_file_candidates(&name).into_iter().next()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Long-poll `/hook/wait`, handing each delivery to `deliver` and then acking it.
/// Returns true after the first delivery when `once`, and false if the parent
/// went away first. Without `once` it runs until killed or orphaned.
fn wait_for_mail(
    session_id: &str,
    cwd: &str,
    base_override: Option<&str>,
    once: bool,
    parent: Option<u32>,
    mut deliver: impl FnMut(&str) -> std::io::Result<()>,
) -> bool {
    let override_address = base_override
        .map(str::to_owned)
        .or_else(|| std::env::var("MARSHAL_BASE_URL").ok());
    if !override_address
        .as_deref()
        .is_some_and(|address| address.starts_with("http://"))
    {
        let address = override_address
            .filter(|address| address.starts_with("ws://") || address.starts_with("wss://"))
            .unwrap_or_else(marshal_client::config::server_address);
        return wait_for_myko_mail(session_id, cwd, address, once, parent, deliver);
    }
    // A one-shot waiter only notices its parent is gone between polls, and until
    // then it could take mail for a closed session, so it polls more often.
    let wait = if once { ONCE_WAIT } else { WATCH_WAIT };
    let body = registration_body(session_id, cwd, agent_id_from_env().as_deref());
    let path = format!(
        "/hook/wait?host={}&operator={}&harness=claude&ack=client&timeout={}",
        url_q(&short_host()),
        url_q(&operator()),
        wait.as_secs()
    );
    let base = resolve_base(base_override);
    // The daemon answers within `WATCH_WAIT`; the read timeout only has to
    // outlast it, so a daemon that vanished mid-wait is noticed.
    let timeout = poll_timeout(wait);
    // Report only a change of cause: stdout would wake the session, and
    // repeating it every retry would just bury the Monitor's output file.
    let mut failing = None;
    // After the daemon says it's stopping, stay quiet about it being unreachable
    // until this deadline, since that's expected while it restarts.
    let mut restart_quiet_until: Option<std::time::Instant> = None;
    loop {
        if parent_pid() != parent {
            return false;
        }
        let post = |path: &str, body: &str, timeout| {
            http_post_why(&base, path, body, Instant::now() + timeout)
        };
        match post(&path, &body, timeout) {
            Ok(reply) => {
                failing = None;
                clear_refusal(session_id);
                if reply.restarting {
                    restart_quiet_until = Some(std::time::Instant::now() + RESTART_QUIET);
                    std::thread::sleep(RESTART_BACKOFF);
                    continue;
                }
                let resp = reply.body.trim_end_matches(['\n', '\r', ' ']);
                if !resp.is_empty() {
                    // The session closed during the poll: leave the mail unacked,
                    // for whoever reads next once the daemon's lease lapses.
                    if parent_pid() != parent {
                        return false;
                    }
                    if deliver(resp).is_err() {
                        return false;
                    }
                    // Only now has it been handed on. A lost ack means the mail
                    // comes again, which beats losing it.
                    if !reply.ack_ids.is_empty() {
                        let ack = serde_json::json!({
                            "session_id": session_id,
                            "message_ids": reply.ack_ids,
                        })
                        .to_string();
                        if let Err(e) = post("/hook/ack", &ack, ACK_TIMEOUT) {
                            eprintln!(
                                "marshal-stdio watch: couldn't ack delivered mail, so it may arrive again: {e}"
                            );
                        }
                    }
                    if once {
                        return true;
                    }
                }
            }
            Err(failure) => {
                // A refusal won't clear by itself, so tell the session once per
                // cause, on the path it reads: Monitor ignores a watcher's stderr.
                // A new waiter or re-armed watcher mustn't tell it again, or
                // every Stop would wake it.
                if failure.is_refusal() && refusal_is_new(session_id, failure) {
                    if deliver(&format!(
                        "Marshal can't deliver mail to this session: {failure}. Please tell your operator. This notice won't repeat."
                    )).is_err() { return false; }
                    if once {
                        return true;
                    }
                }
                // A one-shot waiter stops polling on a refusal. Its stderr reaches
                // the model with its next message, by which time an outage would
                // be long out of date, so it stays quiet about those.
                if once && failure.is_refusal() {
                    return false;
                }
                let restarting =
                    restart_quiet_until.is_some_and(|until| std::time::Instant::now() < until);
                if failing != Some(failure) && !once && !restarting {
                    eprintln!("marshal-stdio watch: {failure}; retrying");
                    failing = Some(failure);
                }
                std::thread::sleep(WATCH_RETRY);
            }
        }
    }
}

fn wait_for_myko_mail(
    session_id: &str,
    cwd: &str,
    address: String,
    once: bool,
    parent: Option<u32>,
    mut deliver: impl FnMut(&str) -> std::io::Result<()>,
) -> bool {
    use hyphae::{Gettable as _, Watchable as _};
    use marshal_entities::{
        AckMessages, AckMessagesResult, GetMessageReadsByQuery, GetMessagesByQuery, HostInfo,
        MessageQuery, MessageReadQuery, RegisterHarnessSession, SessionId, SetHarnessWake,
    };
    use myko::query::IdFilter;
    use std::sync::Arc;

    let repository = marshal_client::config::repository_key(cwd);
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async {
        let session = SessionId(Arc::from(session_id));
        let client = marshal_client::MarshalClient::new();
        let messages = client.watch_query::<GetMessagesByQuery>(GetMessagesByQuery(MessageQuery {
            to_session_id: Some(IdFilter::Eq(session.clone())),
            ..Default::default()
        }));
        let reads = client.watch_query::<GetMessageReadsByQuery>(GetMessageReadsByQuery(
            MessageReadQuery {
                session_id: Some(IdFilter::Eq(session.clone())),
                ..Default::default()
            },
        ));
        let changed = Arc::new(tokio::sync::Notify::new());
        let message_changed = changed.clone();
        let read_changed = changed.clone();
        let _message_guard = messages.subscribe(move |_| message_changed.notify_one());
        let _read_guard = reads.subscribe(move |_| read_changed.notify_one());
        client.set_address(Some(address));
        let set_wake = |waiting| {
            client.send_command::<SetHarnessWake, ()>(&SetHarnessWake {
                session_id: session.clone(),
                waiting,
            })
        };
        let mut armed = false;
        let mut refresh_at = Instant::now();
        let mut next_inbox_retry = Instant::now();
        loop {
            if parent_pid() != parent {
                return false;
            }
            if matches!(
                client.connection_status().get(),
                marshal_client::ConnectionStatus::Connected(_)
            ) {
                if !armed {
                    let registered = client.send_command::<RegisterHarnessSession, String>(
                        &RegisterHarnessSession {
                            session_id: session.clone(),
                            cwd: cwd.into(),
                            repository: repository.clone(),
                            agent_id: agent_id_from_env(),
                            operator: Some(operator()),
                            host: Some(HostInfo {
                                name: short_host(),
                                os: std::env::consts::OS.into(),
                                arch: std::env::consts::ARCH.into(),
                            }),
                            activity: None,
                            waiting: true,
                        },
                    );
                    armed = await_command(|| registered.get()).await;
                    refresh_at = Instant::now();
                }
                if armed && Instant::now() >= refresh_at {
                    let response = set_wake(true);
                    armed = await_command(|| response.get()).await;
                    refresh_at = Instant::now() + Duration::from_secs(30);
                }
                let read = reads.get();
                if armed
                    && Instant::now() >= next_inbox_retry
                    && messages
                        .get()
                        .iter()
                        .any(|message| !read.iter().any(|mark| mark.message_id == message.id))
                {
                    next_inbox_retry = Instant::now() + Duration::from_secs(2);
                    let inbox = tokio::time::timeout(
                        Duration::from_secs(3),
                        marshal_client::hooks::request_harness_inbox(
                            &client, session_id, false, false,
                        ),
                    )
                    .await
                    .ok()
                    .flatten();
                    if let Some(inbox) = inbox
                        && !inbox.context.is_empty()
                    {
                        if parent_pid() != parent || deliver(&inbox.context).is_err() {
                            return false;
                        }
                        let ack =
                            client.send_command::<AckMessages, AckMessagesResult>(&AckMessages {
                                message_ids: inbox.message_ids,
                                as_session: Some(session.clone()),
                            });
                        let _ = await_command(|| ack.get()).await;
                        if once {
                            let response = set_wake(false);
                            let _ = await_command(|| response.get()).await;
                            return true;
                        }
                    }
                }
            } else {
                armed = false;
            }
            tokio::select! {
                () = changed.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    })
}

async fn await_command<T>(get: impl Fn() -> Option<Result<T, String>>) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(result) = get() {
                return result.is_ok();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or(false)
}

fn refusal_marker(session_id: &str) -> Option<std::path::PathBuf> {
    config_file_candidates(&format!("refused-{session_id}"))
        .into_iter()
        .next()
}

/// Whether this session hasn't yet been told of this cause, recording that it
/// now has. If the marker can't be written, it says no: staying quiet beats
/// waking the session at every Stop.
fn refusal_is_new(session_id: &str, failure: PostFailure) -> bool {
    let Some(path) = refusal_marker(session_id) else {
        return false;
    };
    let cause = failure.to_string();
    if std::fs::read_to_string(&path).is_ok_and(|seen| seen == cause) {
        return false;
    }
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return false;
    }
    std::fs::write(&path, cause).is_ok()
}

fn clear_refusal(session_id: &str) {
    if let Some(path) = refusal_marker(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// This process's parent, which changes when the parent exits. `None` where
/// that can't be checked, so `watch --once` simply keeps running there.
fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// How long one `watch` long-poll asks the daemon to hold, and how long to
/// back off when the daemon can't be reached.
const WATCH_WAIT: Duration = Duration::from_secs(240);
const ONCE_WAIT: Duration = Duration::from_secs(30);
const WATCH_RETRY: Duration = Duration::from_secs(5);
/// How long to pause after the daemon ends a wait because it's stopping, and
/// how long its being unreachable goes unreported afterwards.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const RESTART_QUIET: Duration = Duration::from_secs(30);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY: Duration = Duration::from_secs(1);
const POLL_RESPONSE_GRACE: Duration = Duration::from_secs(30);
const TAKEOVER_SLACK: Duration = Duration::from_secs(5);

fn poll_timeout(wait: Duration) -> Duration {
    wait + POLL_RESPONSE_GRACE
}

fn default_lock_wait() -> Duration {
    poll_timeout(ONCE_WAIT) + WATCH_RETRY + TAKEOVER_SLACK
}

/// Cover the full request, backoff and coarse OS socket-timeout scheduling.
fn lock_wait() -> Duration {
    std::env::var("MARSHAL_WAITER_LOCK_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or_else(default_lock_wait, Duration::from_secs)
}
/// Longest line `watch` prints. Monitor cuts an event's line at about 500
/// characters, so a longer one would reach the session with its end missing.
const WATCH_LINE_CHARS: usize = 400;
/// Longest single Monitor event. Monitor also cuts a whole event at about 3,000
/// characters, and joins lines printed within 200ms into one event, so a large
/// batch goes out in pieces with a pause between them.
const WATCH_EVENT_CHARS: usize = 2_500;
const WATCH_EVENT_GAP: Duration = Duration::from_millis(400);

/// Group whole lines into chunks of at most `max` characters. A single longer
/// line gets its own chunk; `wrap_long_lines` keeps that from happening here.
fn event_chunks(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    for line in text.split('\n') {
        let chars = line.chars().count();
        if current_chars > 0 && current_chars + 1 + chars > max {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += chars;
    }
    chunks.push(current);
    chunks
}

/// Break lines longer than `max` characters, at a space when there is one.
fn wrap_long_lines(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while let Some((limit, _)) = rest.char_indices().nth(max) {
            // A space at the very start would leave an empty line behind.
            let cut = match rest[..limit].rfind(' ') {
                Some(space) if space > 0 => space,
                _ => limit,
            };
            out.push_str(&rest[..cut]);
            out.push('\n');
            rest = rest[cut..].strip_prefix(' ').unwrap_or(&rest[cut..]);
        }
        out.push_str(rest);
    }
    out
}

fn write_watch_events(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    let wrapped = wrap_long_lines(text, WATCH_LINE_CHARS);
    for (i, chunk) in event_chunks(&wrapped, WATCH_EVENT_CHARS).iter().enumerate() {
        if i > 0 {
            std::thread::sleep(WATCH_EVENT_GAP);
        }
        writeln!(out, "{chunk}")?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn takeover_budget_outlasts_the_full_old_poll_and_error_backoff() {
        assert!(default_lock_wait() > poll_timeout(ONCE_WAIT) + WATCH_RETRY);
        assert_eq!(default_lock_wait(), Duration::from_secs(70));
    }

    #[cfg(unix)]
    #[test]
    fn an_orphaned_lock_waiter_returns_without_waiting_for_the_deadline() {
        let started = Instant::now();
        assert!(
            acquire_waiter_lock(
                "orphaned-lock-test",
                started + Duration::from_secs(10),
                Some(u32::MAX)
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct FailingWriter {
        writes_left: usize,
        fail_flush: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.writes_left == 0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.writes_left -= 1;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn watch_output_propagates_first_write_later_write_and_flush_failures() {
        for mut out in [
            FailingWriter {
                writes_left: 0,
                fail_flush: false,
            },
            FailingWriter {
                writes_left: 1,
                fail_flush: false,
            },
            FailingWriter {
                writes_left: usize::MAX,
                fail_flush: true,
            },
        ] {
            assert!(write_watch_events(&mut out, &"word ".repeat(1000)).is_err());
        }
    }

    #[test]
    fn event_chunks_keep_lines_whole_and_under_the_limit() {
        assert_eq!(
            event_chunks("aaaa\nbbbb\ncc\ndddddd", 9),
            ["aaaa\nbbbb", "cc\ndddddd"]
        );
    }

    #[test]
    fn event_chunks_lose_nothing() {
        let text = "one\n\ntwo\nthree";
        assert_eq!(event_chunks(text, 5).join("\n"), text);
        assert_eq!(event_chunks("", 5), [""]);
    }

    #[test]
    fn wrap_long_lines_keeps_every_word_under_the_limit() {
        let text = "one two three four\nshort";
        let wrapped = wrap_long_lines(text, 9);
        assert_eq!(wrapped, "one two\nthree\nfour\nshort");
    }

    #[test]
    fn wrap_long_lines_splits_a_long_word_on_a_char_boundary() {
        let wrapped = wrap_long_lines("ééééé", 2);
        assert_eq!(wrapped, "éé\néé\né");
    }

    #[test]
    fn wrap_long_lines_leaves_short_text_alone() {
        let text = "<marshal_inbox count=\"1\">\n- from a: hi\n</marshal_inbox>";
        assert_eq!(wrap_long_lines(text, 400), text);
    }
}
