//! Runs `marshal-launch-ssh` end to end, with `tests/fixtures/fake-ssh`
//! standing in for ssh so the adapter's real remote scripts run on this
//! machine. tmux runs on a private socket under a temp dir.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use marshal_launch::PROTOCOL_VERSION;
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("tmux")).unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        Env { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn bin(&self, name: &str, body: &str) {
        let p = self.path("bin").join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run(&self, verb: &str, req: &Value, extra: &[(&str, &str)]) -> Output {
        let request = if req.get("harness").is_some() {
            let process: marshal_launch::ProcessRequest =
                serde_json::from_value(req.clone()).unwrap();
            serde_json::to_value(marshal_launch::LaunchRequest::from(process)).unwrap()
        } else {
            req.clone()
        };
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_marshal-launch-ssh"));
        c.arg(verb)
            .env("MARSHAL_LAUNCH_SSH", fixture("fake-ssh"))
            .env(
                "MARSHAL_LAUNCH_SSH_HELPER",
                env!("CARGO_BIN_EXE_marshal-launch-ssh"),
            )
            .env("HOME", self.path("home"))
            .env("TMUX_TMPDIR", self.path("tmux"))
            .env_remove("TMUX")
            .env_remove("MARSHAL_BASE_URL")
            // The "target's" login shell: bash, unless a test says otherwise.
            .env("SHELL", which("bash"))
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn request(&self, argv: &[&str]) -> Value {
        json!({
            "version": PROTOCOL_VERSION,
            "harness": "claude",
            "argv": argv,
            "cwd": self.path("work"),
            "name": "launch-test",
            "launch_id": "launch-test-1",
            "marshal_version": marshal_launch_ssh::VERSION,
            "env": {
                "MARSHAL_AGENT_ID": "launch-test-1",
                "MARSHAL_DAEMON_ADDRESS": "ws://marshal.invalid:6155",
            },
            "options": ["--host", "target-1"],
        })
    }

    fn tmux_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(args)
            .env("TMUX_TMPDIR", self.path("tmux"))
            .env_remove("TMUX");
        command
    }

    fn tmux(&self, args: &[&str]) -> Output {
        self.tmux_command(args).output().expect("run fixture tmux")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.tmux_command(&["kill-server"]).output();
    }
}

fn which(p: &str) -> String {
    let o = Command::new("sh")
        .args(["-c", &format!("command -v {p}")])
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout_json(o: &Output) -> Value {
    assert!(o.status.success(), "exit {:?}: {}", o.status, stderr(o));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn have_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn launch_runs_argv_in_cwd_with_env_then_status_and_stop() {
    if !have_tmux() {
        eprintln!("skipping: no tmux");
        return;
    }
    let e = Env::new();
    // Prints what it was given, so the test sees argv, env and cwd unchanged.
    let req = e.request(&[
        "sh",
        "-c",
        "printf '%s|%s|%s|%s\\n' \"$MARSHAL_AGENT_ID\" \"$MARSHAL_DAEMON_ADDRESS\" \"$(pwd -P)\" \"$1\" > out; sleep 60",
        "sh",
        "it's one arg",
    ]);
    let l = stdout_json(&e.run("launch", &req, &[]));
    let work = std::fs::canonicalize(e.path("work")).unwrap();
    let host = String::from_utf8(Command::new("uname").arg("-n").output().unwrap().stdout).unwrap();
    assert_eq!(l["version"], PROTOCOL_VERSION);
    assert_eq!(l["handle"], "target-1 marshal-launch-test-1");
    assert_eq!(l["host"], host.trim());
    assert_eq!(l["cwd"], work.display().to_string());
    assert_eq!(l["where"], format!("{} tmux:launch-test", host.trim()));
    let attach = l["attach"].as_str().unwrap();
    assert!(attach.contains(&which("tmux")));
    assert!(attach.contains("=marshal-launch-test-1"));
    // The response parses as the launcher's type.
    let _: marshal_launch::Launched = serde_json::from_value(l.clone()).unwrap();

    let out = work.join("out");
    for _ in 0..100 {
        if std::fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n')) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!(
            "launch-test-1|ws://marshal.invalid:6155|{}|it's one arg\n",
            work.display()
        )
    );

    assert_eq!(
        stdout_json(&e.run("recover", &req, &[]))["handle"],
        l["handle"]
    );
    assert_eq!(
        stdout_json(&e.run("inspect", &req, &[]))["launched"]["handle"],
        l["handle"]
    );
    assert_eq!(stdout_json(&e.run("launch", &req, &[])), l);

    let h = json!({"version": PROTOCOL_VERSION, "handle": l["handle"]});
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], true);
    assert_eq!(stdout_json(&e.run("stop", &h, &[]))["state"], "stopped");
    assert_eq!(stdout_json(&e.run("status", &h, &[]))["running"], false);

    let mut replacement = req.clone();
    replacement["launch_id"] = json!("launch-replacement");
    let other = stdout_json(&e.run("launch", &replacement, &[]));
    let other_handle = json!({"version": PROTOCOL_VERSION, "handle": other["handle"]});
    assert_eq!(stdout_json(&e.run("stop", &h, &[]))["state"], "gone");
    assert_eq!(
        stdout_json(&e.run("status", &other_handle, &[]))["running"],
        true
    );

    e.bin("tmux", "echo inaccessible >&2; exit 1");
    assert!(!e.run("stop", &other_handle, &[]).status.success());
    std::fs::remove_file(e.path("bin/tmux")).unwrap();
    assert_eq!(
        stdout_json(&e.run("status", &other_handle, &[]))["running"],
        true
    );
    assert_eq!(
        stdout_json(&e.run("stop", &other_handle, &[]))["state"],
        "stopped"
    );
}

#[test]
fn missing_or_incompatible_helper_never_falls_back_to_raw_tmux() {
    let e = Env::new();
    let req = e.request(&["sh", "-c", "touch must-not-start; sleep 30"]);
    let missing = e.run(
        "launch",
        &req,
        &[("MARSHAL_LAUNCH_SSH_HELPER", "/no-such-marshal-helper")],
    );
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("install marshal-launch-ssh"),
        "{}",
        stderr(&missing)
    );
    e.bin(
        "incompatible-helper",
        "printf '%s\\n' '@@marshal-target {\"version\":2,\"helper_version\":\"wrong\"}'",
    );
    let helper = e.path("bin/incompatible-helper");
    let incompatible = e.run(
        "launch",
        &req,
        &[("MARSHAL_LAUNCH_SSH_HELPER", helper.to_str().unwrap())],
    );
    assert!(!incompatible.status.success());
    assert!(
        stderr(&incompatible).contains("version mismatch"),
        "{}",
        stderr(&incompatible)
    );
    assert!(!e.path("work/must-not-start").exists());
    assert!(
        !e.tmux(&["has-session", "-t", "=marshal-launch-test-1"])
            .status
            .success()
    );
}

#[test]
fn daemon_credentials_are_removed_before_ssh_and_from_target_json() {
    let e = Env::new();
    let request_file = e.path("request.json");
    e.bin("record-ssh", "[ -z \"${MYKO_POSTGRES_URL+present}\" ] || exit 73\ncat > \"$FAKE_REQUEST\"\nprintf '%s\\n' '@@marshal-target {\"version\":2,\"helper_version\":\"wrong\"}'");
    let mut req = e.request(&["sh", "-c", "sleep 30"]);
    req["env"]["MYKO_POSTGRES_URL"] = json!("synthetic-request-credential");
    req["env"]["MARSHAL_AUTH_CLIENT_SECRET"] = json!("synthetic-client-secret");
    let output = e.run(
        "launch",
        &req,
        &[
            (
                "MARSHAL_LAUNCH_SSH",
                e.path("bin/record-ssh").to_str().unwrap(),
            ),
            ("FAKE_REQUEST", request_file.to_str().unwrap()),
            ("MYKO_POSTGRES_URL", "synthetic-process-credential"),
        ],
    );
    assert!(!output.status.success());
    let wire = std::fs::read_to_string(request_file).unwrap();
    assert!(!wire.contains("synthetic-request-credential"));
    assert!(!wire.contains("synthetic-client-secret"));
    let wire: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(wire["env"]["MARSHAL_AGENT_ID"], "launch-test-1");
    assert_eq!(wire["argv"], req["argv"]);
}

#[test]
fn stop_rechecks_process_ownership_inside_tmux() {
    if !have_tmux() {
        return;
    }
    let e = Env::new();
    let req = e.request(&["sh", "-c", "sleep 60"]);
    let launch = stdout_json(&e.run("launch", &req, &[]));
    let handle = json!({"version": PROTOCOL_VERSION, "handle": launch["handle"]});
    let real = which("tmux");
    e.bin("tmux", &format!(r#"
        for arg do case "$arg" in
          if-shell|kill-session)
            "{real}" set-environment -t '=marshal-launch-test-1' MARSHAL_LAUNCH_KEY different-owner ;;
        esac; done
        exec "{real}" "$@"
    "#));
    let stop = e.run("stop", &handle, &[]);
    assert!(
        !stop.status.success(),
        "stop succeeded after the ownership marker changed"
    );
    assert!(
        e.tmux(&["has-session", "-t", "=marshal-launch-test-1"])
            .status
            .success()
    );

    std::fs::remove_file(e.path("bin/tmux")).unwrap();
    assert!(
        e.tmux(&[
            "set-environment",
            "-t",
            "=marshal-launch-test-1",
            "MARSHAL_LAUNCH_KEY",
            "marshal-launch-test-1"
        ])
        .status
        .success()
    );
    assert_eq!(
        stdout_json(&e.run("stop", &handle, &[]))["state"],
        "stopped"
    );
}

#[test]
fn launch_refuses_loopback_and_starts_nothing() {
    let e = Env::new();
    let transcript = e.path("ssh-transcript");
    let env = [("FAKE_SSH_TRANSCRIPT", transcript.to_str().unwrap())];
    let mut req = e.request(&["true"]);
    req["env"]["MARSHAL_DAEMON_ADDRESS"] = json!("ws://localhost:6155");
    for verb in ["check", "launch"] {
        let o = e.run(verb, &req, &env);
        assert_eq!(o.status.code(), Some(1));
        assert!(stderr(&o).contains("loopback"), "{}", stderr(&o));
        assert!(o.stdout.is_empty());
    }
    assert!(
        !transcript.exists(),
        "the request reached the fake SSH target"
    );
}

#[test]
fn an_unreachable_target_starts_nothing_and_says_so() {
    let e = Env::new();
    let tmux_invoked = e.path("tmux-invoked");
    e.bin("tmux", r#": > "$FAKE_TMUX_INVOKED"; exit 1"#);
    let o = e.run(
        "launch",
        &e.request(&["true"]),
        &[
            ("FAKE_SSH_UNREACHABLE", "1"),
            ("FAKE_TMUX_INVOKED", tmux_invoked.to_str().unwrap()),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(err.contains("No route to host"), "{err}");
    assert!(
        err.contains("ssh to target-1 did not return a result"),
        "{err}"
    );
    assert!(
        !tmux_invoked.exists(),
        "the request started tmux on the fake target"
    );
}

#[test]
fn check_runs_its_probes_on_the_target_and_names_each_failure() {
    let e = Env::new();
    e.bin("marshal-shim", "echo 'marshal-shim 0.1.0'");
    e.bin("claude", "echo '[]'");
    let mut req = e.request(&["claude"]);
    req["cwd"] = json!("~/no-such-folder");
    let o = e.run("check", &req, &[]);
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    for want in [
        "can't reach the daemon at marshal.invalid:6155",
        "can't reach the daemon's hook listener at marshal.invalid:6156",
        "MARSHAL_HOOK_BIND",
        "has no folder",
        "no-such-folder",
        "the target's marshal-shim is 0.1.0, but this launcher is",
        "has no MCP server that runs marshal-shim",
        "don't run `marshal-shim claude-hook`",
    ] {
        assert!(err.contains(want), "missing `{want}` in:\n{err}");
    }
    // `~` is the target's home.
    assert!(
        err.contains(&e.path("home").join("no-such-folder").display().to_string()),
        "{err}"
    );
}

#[test]
fn check_reads_a_wired_claude_plugin_on_the_target() {
    let e = Env::new();
    let plugin = e.path("home/.claude/plugins/cache/marshal/marshal-shim/0.60.2");
    std::fs::create_dir_all(plugin.join("hooks")).unwrap();
    let repo_plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/marshal-shim");
    std::fs::copy(
        repo_plugin.join("hooks/hooks.json"),
        plugin.join("hooks/hooks.json"),
    )
    .unwrap();
    std::fs::copy(repo_plugin.join(".mcp.json"), plugin.join(".mcp.json")).unwrap();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin(
        "claude",
        &format!(
            "echo '[{{\"id\":\"marshal-shim@marshal\",\"enabled\":true,\"installPath\":\"{}\"}}]'",
            plugin.display()
        ),
    );
    let o = e.run("check", &e.request(&["claude"]), &[]);
    let err = stderr(&o);
    // Only the ports fail: marshal.invalid doesn't resolve. The plugin
    // shipped in this repo counts as wired.
    assert!(err.contains("can't reach the daemon at"), "{err}");
    assert!(!err.contains("plugin"), "{err}");
    assert!(!err.contains("Claude's hooks"), "{err}");
    assert!(!err.contains("MCP"), "{err}");
    assert!(!err.contains("the target's marshal-shim is"), "{err}");
}

#[test]
fn check_fails_when_the_targets_daemon_address_file_names_another_daemon() {
    let e = Env::new();
    let write = |dir: PathBuf, addr: &str| {
        std::fs::create_dir_all(dir.join("marshal")).unwrap();
        std::fs::write(dir.join("marshal/daemon-address"), format!("  {addr}\n")).unwrap();
    };
    // The target's own ~/.config.
    let home_cfg = e.path("home/.config");
    write(home_cfg.clone(), "ws://elsewhere:6155");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    let file = home_cfg.join("marshal/daemon-address");
    assert!(
        err.contains(&format!(
            "has {} naming ws://elsewhere:6155",
            file.display()
        )),
        "{err}"
    );
    assert!(err.contains("ahead of MARSHAL_DAEMON_ADDRESS"), "{err}");

    // A file naming the launch address is fine.
    write(home_cfg.clone(), "ws://marshal.invalid:6155");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(!err.contains("daemon-address"), "{err}");

    // The request's XDG_CONFIG_HOME is the agent's, so its file wins.
    let xdg = e.path("xdg");
    write(xdg.clone(), "ws://other:7000");
    let mut req = e.request(&["claude"]);
    req["env"]["XDG_CONFIG_HOME"] = serde_json::json!(xdg);
    let err = stderr(&e.run("check", &req, &[]));
    assert!(err.contains("naming ws://other:7000"), "{err}");
}

fn have_json_reader() -> bool {
    ["python3", "jq"].iter().any(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Wired by hand, as upstream's README documents: the MCP server in
/// ~/.claude.json and the hooks in ~/.claude/settings.json.
#[test]
fn check_accepts_claude_wired_by_hand_once_it_has_the_wait_hook() {
    if !have_json_reader() {
        eprintln!("skipping: neither python3 nor jq");
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin("claude", "echo '[]'");
    std::fs::write(
        e.path("home/.claude.json"),
        r#"{"numStartups": 3, "mcpServers": {"marshal": {"command": "marshal-shim"}},
            "projects": {"/elsewhere": {"history": ["kept on the target"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(e.path("home/.claude")).unwrap();
    let settings = |extra: &str| {
        std::fs::write(
            e.path("home/.claude/settings.json"),
            format!(
                r#"{{"env": {{"SOME_TOKEN": "stays-on-the-target"}}, "hooks": {{
                  "SessionStart": [{{"hooks": [{{"type": "command", "command": "marshal-shim claude-hook session-start", "timeout": 15}}]}}],
                  "UserPromptSubmit": [{{"hooks": [{{"type": "command", "command": "marshal-shim claude-hook prompt-submit", "timeout": 15}}]}}]{extra}
                }}}}"#
            ),
        )
        .unwrap()
    };

    // The README's hooks alone: the only Claude reason is the missing waiter.
    settings("");
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(err.contains("not `marshal-shim claude-hook wait`"), "{err}");
    assert!(!err.contains("MCP server"), "{err}");
    assert!(!err.contains("don't run"), "{err}");

    settings(
        r#", "Stop": [{"hooks": [{"type": "command", "command": "marshal-shim claude-hook wait", "asyncRewake": true}]}]"#,
    );
    let err = stderr(&e.run("check", &e.request(&["claude"]), &[]));
    assert!(err.contains("can't reach the daemon at"), "{err}");
    assert!(!err.contains("Claude"), "{err}");
}

#[test]
fn preflight_never_executes_project_controlled_shim() {
    if !have_json_reader() {
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin("claude", "echo '[]'");
    std::fs::create_dir_all(e.path("work/.claude")).unwrap();
    std::fs::write(
        e.path("work/.claude/settings.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"./marshal-shim claude-hook wait"}]}]}}"#,
    )
    .unwrap();
    std::fs::write(
        e.path("work/.mcp.json"),
        r#"{"mcpServers":{"marshal":{"command":"./marshal-shim"}}}"#,
    )
    .unwrap();
    e.bin("project-shim", "touch EXECUTED; echo 'marshal-shim 0.0.0'");
    std::fs::copy(e.path("bin/project-shim"), e.path("work/marshal-shim")).unwrap();
    let out = e.run("check", &e.request(&["claude"]), &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("outside the trusted marshal-shim installation"),
        "{}",
        stderr(&out)
    );
    assert!(!e.path("work/EXECUTED").exists());
}

#[test]
fn codex_preflight_compares_hook_routes_without_exporting_config() {
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    let dir = e.path("home/codex");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("config.toml");
    let transcript = e.path("transcript");
    let env = [
        ("CODEX_HOME", dir.to_str().unwrap()),
        ("FAKE_SSH_TRANSCRIPT", transcript.to_str().unwrap()),
    ];
    let mut request = e.request(&["marshal-shim", "codex-run"]);
    request["harness"] = json!("codex");
    let config = |hook, end| {
        format!(
            r#"# >>> marshal (managed by marshal-shim codex-setup) >>>
[mcp_servers.marshal]
command = "marshal-shim"
env = {{ MARSHAL_DAEMON_ADDRESS = "ws://marshal.invalid:6155", TOKEN = "SYNTHETIC_SECRET" }}
[[hooks.SessionStart]]
command = "marshal-shim codex-hook session-start {hook}"
{end}
[mcp_servers.unrelated.env]
TOKEN = "UNRELATED_SECRET"
"#
        )
    };
    let end = "# <<< marshal (managed by marshal-shim codex-setup) <<<";
    std::fs::write(&file, config("http://marshal.invalid:6156", end)).unwrap();
    let out = e.run("check", &request, &env);
    assert!(
        stderr(&out).contains("can't reach the daemon"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("Codex"), "{}", stderr(&out));
    std::fs::write(&file, config("http://other.invalid:6156", end)).unwrap();
    let out = e.run("check", &request, &env);
    assert!(
        stderr(&out).contains("target a different hook URL"),
        "{}",
        stderr(&out)
    );

    std::fs::write(&file, config("http://marshal.invalid:6156", "")).unwrap();
    let out = e.run("check", &request, &env);
    assert!(
        stderr(&out).contains("no complete marshal block"),
        "{}",
        stderr(&out)
    );
    let sent = std::fs::read_to_string(transcript).unwrap();
    assert!(!sent.contains("SYNTHETIC_SECRET"));
    assert!(!sent.contains("UNRELATED_SECRET"));
}

/// The shim there takes MARSHAL_BASE_URL from the target's own environment
/// when the request sets none, so the check probes that hook listener.
#[test]
fn check_probes_the_hook_listener_the_targets_marshal_base_url_names() {
    let e = Env::new();
    for (base, want) in [
        (
            "http://hooks.invalid:7001/",
            "hook listener at hooks.invalid:7001",
        ),
        ("http://[fd00::1]:7002", "hook listener at [fd00::1]:7002"),
        (
            "https://hooks.invalid",
            "hook listener at hooks.invalid:443",
        ),
    ] {
        let err = stderr(&e.run(
            "check",
            &e.request(&["claude"]),
            &[("MARSHAL_BASE_URL", base)],
        ));
        assert!(err.contains(want), "{base}: {err}");
    }
    // The request's own MARSHAL_BASE_URL wins over the target's.
    let mut req = e.request(&["claude"]);
    req["env"]["MARSHAL_BASE_URL"] = json!("http://chosen.invalid:7003");
    let err = stderr(&e.run(
        "check",
        &req,
        &[("MARSHAL_BASE_URL", "http://hooks.invalid:7001")],
    ));
    assert!(
        err.contains("hook listener at chosen.invalid:7003"),
        "{err}"
    );
}

/// The check's probes connect: listeners on this machine's non-loopback
/// address are reported reachable (loopback launch addresses are refused).
#[test]
fn check_reaches_listening_ports() {
    let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("10.255.255.255:9").map(|_| s))
        .and_then(|s| s.local_addr())
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    else {
        eprintln!("skipping: no non-loopback address");
        return;
    };
    let daemon = std::net::TcpListener::bind((ip, 0)).unwrap();
    let hook = std::net::TcpListener::bind((ip, 0)).unwrap();
    let e = Env::new();
    let mut req = e.request(&["claude"]);
    req["env"]["MARSHAL_DAEMON_ADDRESS"] =
        json!(format!("ws://{ip}:{}", daemon.local_addr().unwrap().port()));
    req["env"]["MARSHAL_BASE_URL"] =
        json!(format!("http://{ip}:{}", hook.local_addr().unwrap().port()));
    let err = stderr(&e.run("check", &req, &[]));
    assert!(err.contains("can't run this launch"), "{err}");
    assert!(!err.contains("can't reach the daemon"), "{err}");
}

/// Rule 2: check brings back facts about the wiring, never the target's
/// secrets. Other MCP servers' env, settings env, hook arguments and
/// history stay on the target.
#[test]
fn check_brings_no_secrets_back_from_the_target() {
    if !have_json_reader() {
        eprintln!("skipping: neither python3 nor jq");
        return;
    }
    let e = Env::new();
    e.bin(
        "marshal-shim",
        &format!("echo 'marshal-shim {}'", marshal_launch_ssh::VERSION),
    );
    e.bin(
        "claude",
        "echo '[{\"id\":\"other@x\",\"enabled\":true,\"installPath\":\"/p/SECRET-PLUGIN-PATH\"}]'",
    );
    std::fs::write(
        e.path("home/.claude.json"),
        r#"{"oauthAccount": {"accessToken": "SECRET-OAUTH-TOKEN"},
            "mcpServers": {
              "marshal": {"command": "marshal-shim", "env": {"API_KEY": "SECRET-MARSHAL-ENV"}},
              "github": {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "SECRET-GITHUB-TOKEN"}}},
            "projects": {"/elsewhere": {"history": ["SECRET-HISTORY"]}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(e.path("home/.claude")).unwrap();
    std::fs::write(
        e.path("home/.claude/settings.json"),
        r#"{"env": {"ANTHROPIC_API_KEY": "SECRET-ANTHROPIC-KEY"}, "hooks": {
              "SessionStart": [{"hooks": [
                {"type": "command", "command": "marshal-shim claude-hook session-start --token SECRET-HOOK-ARG"},
                {"type": "command", "command": "TOKEN=SECRET-OTHER-HOOK other-tool run"}]}]}}"#,
    )
    .unwrap();
    let transcript = e.path("transcript");
    let o = e.run(
        "check",
        &e.request(&["claude"]),
        &[("FAKE_SSH_TRANSCRIPT", transcript.to_str().unwrap())],
    );
    let wire = std::fs::read_to_string(&transcript).unwrap();
    // The facts check needs did come back.
    assert!(
        wire.contains("@@claude-hook session-start\tmarshal-shim\t"),
        "{wire}"
    );
    assert!(
        wire.contains("@@claude-mcp user\tmarshal\tmarshal-shim\t\t~/.claude.json"),
        "{wire}"
    );
    let shown = format!("{wire}{}{}", String::from_utf8_lossy(&o.stdout), stderr(&o));
    assert!(
        !shown.contains("SECRET"),
        "a secret left the target:\n{shown}"
    );
}

/// A login shell other than bash (zsh on macOS) sets PATH in its profile.
/// The scripts take PATH from it, so what's only on that PATH is found.
#[test]
fn the_targets_login_shell_supplies_path() {
    let e = Env::new();
    let login_bin = e.path("login-bin");
    std::fs::create_dir_all(&login_bin).unwrap();
    let shim = login_bin.join("marshal-shim");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho 'marshal-shim {}'\n",
            marshal_launch_ssh::VERSION
        ),
    )
    .unwrap();
    let login_sh = e.path("login-sh");
    std::fs::write(
        &login_sh,
        format!(
            "#!/bin/sh\n[ \"$1\" = -lc ] || exit 1\nPATH={}:$PATH\nexport PATH\nexec sh -c \"$2\"\n",
            login_bin.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    for f in [&shim, &login_sh] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let req = e.request(&["claude"]);
    // Only the test's own bin and the system dirs: no machine install of
    // marshal-shim (~/.cargo/bin, ~/.local/bin) leaks in.
    let path = format!("{}:/usr/bin:/bin", e.path("bin").display());
    // With bash as the login shell, the shim isn't on PATH.
    let err = stderr(&e.run("check", &req, &[("PATH", &path)]));
    assert!(err.contains("no marshal-shim on its login PATH"), "{err}");
    // With the other shell, its PATH has it.
    let err = stderr(&e.run(
        "check",
        &req,
        &[("PATH", &path), ("SHELL", login_sh.to_str().unwrap())],
    ));
    assert!(!err.contains("marshal-shim on its login PATH"), "{err}");
    assert!(!err.contains("the target's marshal-shim is"), "{err}");
}

/// A target whose login shell is zsh (the macOS default). Real ssh hands the
/// remote command to the login shell, and the user's attach command goes the
/// same way. zsh expands an unquoted `=word` to a command path (EQUALS), so
/// `-t =<name>` breaks there.
/// This runs the adapter's scripts through `zsh -c` (as sshd would) with zsh
/// as the login shell, on a live session.
///
/// Ignored by default so a machine without zsh or tmux can't count it as a
/// pass; run it with `--ignored`, and it fails if either is missing.
#[test]
#[ignore = "needs zsh and tmux; run with --ignored"]
fn status_stop_and_attach_work_when_the_login_shell_is_zsh() {
    let zsh = which("zsh");
    assert!(
        !zsh.is_empty(),
        "no zsh on this machine, so the zsh login-shell case can't run"
    );
    assert!(
        have_tmux(),
        "no tmux on this machine, so the zsh login-shell case can't run"
    );
    let e = Env::new();
    let shell = [
        ("SHELL", zsh.as_str()),
        ("FAKE_SSH_LOGIN_SHELL", zsh.as_str()),
    ];
    let l = stdout_json(&e.run("launch", &e.request(&["sh", "-c", "sleep 60"]), &shell));
    let h = json!({"version": PROTOCOL_VERSION, "handle": l["handle"]});
    assert_eq!(stdout_json(&e.run("status", &h, &shell))["running"], true);

    let attach = l["attach"].as_str().unwrap();
    let parsed = Command::new("sh")
        .args([
            "-c",
            &format!(r#"ssh() {{ shift 3; printf '%s' "$1"; }}; {attach}"#),
        ])
        .output()
        .unwrap();
    assert!(parsed.status.success());
    let remote = String::from_utf8(parsed.stdout)
        .unwrap()
        .replace(" attach ", " has-session ");
    let in_zsh = |cmd: &str| {
        Command::new(&zsh)
            .args(["-c", cmd])
            .env("TMUX_TMPDIR", e.path("tmux"))
            .env_remove("TMUX")
            .output()
            .unwrap()
    };
    let o = in_zsh(&remote);
    assert!(o.status.success(), "{remote}: {}", stderr(&o));
    // The unquoted form is the bug: zsh fails it before tmux runs.
    let o = in_zsh("tmux has-session -t =launch-test");
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not found"), "{}", stderr(&o));

    stdout_json(&e.run("stop", &h, &shell));
    assert_eq!(stdout_json(&e.run("status", &h, &shell))["running"], false);
}

/// `--oauth-token-file`: the agent gets CLAUDE_CODE_OAUTH_TOKEN, read from a
/// file on the target. The token itself is nowhere in the response, the
/// adapter's output or the tmux session's command; only its path is.
#[test]
fn the_token_file_is_read_on_the_target_and_never_carried() {
    if !have_tmux() {
        eprintln!("skipping: no tmux");
        return;
    }
    let e = Env::new();
    let secret = "sk-ant-oat01-not-a-real-token";
    let file = e.path("home").join("claude-oauth-token");
    std::fs::write(&file, format!("{secret}\n")).unwrap();
    let mut req = e.request(&[
        "sh",
        "-c",
        "printf '%s\\n' \"$CLAUDE_CODE_OAUTH_TOKEN\" > out; sleep 60",
    ]);
    req["options"] = json!([
        "--host",
        "target-1",
        "--oauth-token-file",
        file.display().to_string()
    ]);
    let o = e.run("launch", &req, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), stderr(&o));
    assert!(!said.contains(secret), "{said}");

    let out = e.path("work").join("out");
    for _ in 0..100 {
        if std::fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n')) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{secret}\n")
    );
    let cmd = e.tmux(&[
        "list-panes",
        "-t",
        "=marshal-launch-test-1",
        "-F",
        "#{pane_start_command}",
    ]);
    let cmd = String::from_utf8_lossy(&cmd.stdout);
    assert!(cmd.contains("claude-oauth-token"), "{cmd}");
    assert!(!cmd.contains(secret), "{cmd}");
    let h = json!({"version": PROTOCOL_VERSION, "handle": "target-1 marshal-launch-test-1"});
    stdout_json(&e.run("stop", &h, &[]));

    // Without the file, check names it and launch starts nothing.
    std::fs::remove_file(&file).unwrap();
    let c = e.run("check", &req, &[]);
    assert!(
        stderr(&c).contains(&format!(
            "no readable, non-empty token file at {}",
            file.display()
        )),
        "{}",
        stderr(&c)
    );
    let l = e.run("launch", &req, &[]);
    assert!(!l.status.success());
    assert!(
        stderr(&l).contains("no readable, non-empty token file"),
        "{}",
        stderr(&l)
    );
    assert!(
        !e.tmux(&["has-session", "-t", "=marshal-launch-test-1"])
            .status
            .success()
    );
}

/// On a macOS target, `launch` says how Local Network privacy can keep the
/// agent from joining; elsewhere it says nothing about it.
#[test]
fn launch_on_macos_notes_local_network_privacy() {
    if !have_tmux() {
        eprintln!("skipping: no tmux");
        return;
    }
    let e = Env::new();
    let req = e.request(&["sh", "-c", "sleep 60"]);
    let h = json!({"version": PROTOCOL_VERSION, "handle": "target-1 marshal-launch-test-1"});
    let o = e.run("launch", &req, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!stderr(&o).contains("NECP"), "{}", stderr(&o));
    stdout_json(&e.run("stop", &h, &[]));

    let real = which("uname");
    e.bin(
        "uname",
        &format!("if [ \"$1\" = -s ]; then echo Darwin; else exec {real} \"$@\"; fi"),
    );
    let o = e.run("launch", &req, &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("Local Network") && err.contains("reason: NECP"),
        "{err}"
    );
    stdout_json(&e.run("stop", &h, &[]));
}
