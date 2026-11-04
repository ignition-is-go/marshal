//! Conversation-first review of one imported experiment.

use std::sync::Arc;

use leptos::prelude::*;
use marshal_entities::{SandboxArtifact, SandboxRun, SandboxStatus, SandboxTimelineEvent};
use wasm_bindgen::JsCast;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReviewTab {
    Conversation,
    Compare,
    Context,
}

pub fn outcome(a: &SandboxArtifact) -> (&'static str, &'static str) {
    if a.succeeded() {
        return ("Completed", "sb-good");
    }
    match a.status {
        SandboxStatus::Starting => ("Starting", "sb-neutral"),
        SandboxStatus::Running => ("Running", "sb-neutral"),
        SandboxStatus::Quiescent => ("Incomplete", "sb-bad"),
        SandboxStatus::BudgetExhausted => ("Budget limit", "sb-bad"),
        SandboxStatus::Deadline => ("Deadline", "sb-bad"),
        SandboxStatus::ModelTurnFailed => ("Model failed", "sb-bad"),
        SandboxStatus::Interrupted => ("Interrupted", "sb-neutral"),
        SandboxStatus::Error => ("Run error", "sb-bad"),
    }
}

#[component]
pub fn RunReview(
    artifact: SandboxArtifact,
    runs: Memo<Vec<Arc<SandboxRun>>>,
    baseline: RwSignal<String>,
    tab: RwSignal<ReviewTab>,
) -> impl IntoView {
    let artifact = StoredValue::new(artifact);
    let a = artifact.get_value();
    let (state, tone) = outcome(&a);
    let duration = a.timeline.last().map(|event| event.seconds).unwrap_or(0.0);
    let issues = [
        (!a.tested, "Test unfinished"),
        (!a.released, "Node still held"),
        (a.resource_violations > 0, "Conflicting access"),
        (a.failed_turns > 0, "Model error"),
    ]
    .into_iter()
    .filter_map(|(present, label)| present.then_some(label))
    .collect::<Vec<_>>();
    let cards = [
        ("Messages", a.messages),
        ("Turns", a.turns),
        ("Wake turns", a.wake_turns),
        ("Conflicts", a.resource_violations),
    ];
    view! {
        <article class="sb-review">
            <header class="sb-run-header">
                <div><div class="sb-scenario">{a.scenario.replace('-', " ")}</div><h3>{a.label.clone()}</h3>
                    <p class="sb-run-meta">{a.model.clone()}<span>"/"</span><time>{format!("{duration:.1}s")}</time></p></div>
                <span class=format!("sb-outcome {tone}")><i></i>{state}</span>
            </header>
            {(!issues.is_empty()).then(|| view! { <p class="sb-issues">{issues.join(" · ")}</p> })}
            <div class="sb-stats">{cards.into_iter().map(|(label, value)| view! {
                <div><span>{label}</span><strong>{count(u64::from(value))}</strong></div>
            }).collect_view()}</div>
            <div class="sb-tabs" role="tablist" aria-label="Run review" on:keydown=move_tab>
                {[(ReviewTab::Conversation,"Conversation"),(ReviewTab::Compare,"Compare"),(ReviewTab::Context,"Context")].into_iter().map(|(value,label)| view! {
                    <button role="tab" class:sb-tab-active=move || tab.get() == value aria-selected=move || (tab.get() == value).to_string() tabindex=move || if tab.get() == value { 0 } else { -1 }
                        on:click=move |_| tab.set(value)>{label}</button>
                }).collect_view()}
            </div>
            {move || match tab.get() {
                ReviewTab::Conversation => view! { <Conversation artifact=artifact.get_value() /> }.into_any(),
                ReviewTab::Compare => view! { <Comparison artifact=artifact.get_value() runs=runs baseline=baseline /> }.into_any(),
                ReviewTab::Context => view! { <Context artifact=artifact.get_value() /> }.into_any(),
            }}
        </article>
        <style>{REVIEW_CSS}</style>
    }
}

fn move_tab(event: leptos::ev::KeyboardEvent) {
    let target = event_target::<web_sys::HtmlElement>(&event);
    let parent = target.parent_element();
    let next = match event.key().as_str() {
        "ArrowRight" => target
            .next_element_sibling()
            .or_else(|| parent.as_ref()?.first_element_child()),
        "ArrowLeft" => target
            .previous_element_sibling()
            .or_else(|| parent.as_ref()?.last_element_child()),
        "Home" => parent.as_ref().and_then(|node| node.first_element_child()),
        "End" => parent.as_ref().and_then(|node| node.last_element_child()),
        _ => return,
    };
    event.prevent_default();
    event.stop_propagation();
    if let Some(button) = next.and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok()) {
        button.click();
        let _ = button.focus();
    }
}

#[component]
fn Conversation(artifact: SandboxArtifact) -> impl IntoView {
    let artifact = StoredValue::new(artifact);
    let full_trace = RwSignal::new(false);
    let search = RwSignal::new(String::new());
    let agent = RwSignal::new(String::new());
    let events = Memo::new(move |_| {
        let a = artifact.get_value();
        let query = search.get().to_lowercase();
        let who = agent.get();
        a.timeline
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                let focused = matches!(
                    event.kind.as_str(),
                    "message"
                        | "node_action"
                        | "work_action"
                        | "intervention"
                        | "scenario_check"
                        | "error"
                        | "finished"
                        | "interrupted"
                );
                (full_trace.get() || focused)
                    && (who.is_empty()
                        || event.actor.as_deref() == Some(&who)
                        || event.recipient.as_deref() == Some(&who))
                    && (query.is_empty()
                        || format!(
                            "{} {} {}",
                            event.text,
                            event.kind,
                            event
                                .actor
                                .as_deref()
                                .map(|id| a.actor_name(id))
                                .unwrap_or_default()
                        )
                        .to_lowercase()
                        .contains(&query))
            })
            .map(|(index, event)| (index, event.clone()))
            .collect::<Vec<_>>()
    });
    let a = artifact.get_value();
    view! {
        <div class="sb-conversation">
            <div class="sb-timeline-controls">
                <input type="search" class="sb-search" aria-label="Search timeline" placeholder="Search the conversation…"
                    prop:value=move || search.get() on:input=move |ev| search.set(event_target_value(&ev)) />
                <select aria-label="Filter by agent" prop:value=move || agent.get() on:change=move |ev| agent.set(event_target_value(&ev))>
                    <option value="">"All agents"</option>
                    {a.roles.iter().map(|(role,id)| view! { <option value=id.clone()>{role.clone()}</option> }).collect_view()}
                </select>
                <label class="sb-trace-toggle"><input type="checkbox" aria-label="Include model turns and tools"
                    prop:checked=move || full_trace.get() on:change=move |ev| full_trace.set(event_target_checked(&ev)) />"Full trace"</label>
            </div>
            <ol class="sb-transcript" aria-label="Experiment timeline">
                <For each=move || events.get() key=|(index,_)| *index children=move |(_,event)| {
                    view! { <TimelineRow artifact=artifact event=event /> }
                } />
            </ol>
            <Show when=move || events.get().is_empty()>
                <div class="sb-no-events"><h4>"No matching events"</h4>
                    <button on:click=move |_| { search.set(String::new()); agent.set(String::new()); }>"Clear filters"</button>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn TimelineRow(
    artifact: StoredValue<SandboxArtifact>,
    event: SandboxTimelineEvent,
) -> impl IntoView {
    let actor = event
        .actor
        .as_deref()
        .map(|id| artifact.with_value(|a| a.actor_name(id)))
        .unwrap_or_else(|| "runner".into());
    let recipient = event
        .recipient
        .as_deref()
        .map(|id| format!("→ {}", artifact.with_value(|a| a.actor_name(id))));
    let actor_index = artifact.with_value(|a| {
        a.roles
            .values()
            .position(|id| Some(id.as_str()) == event.actor.as_deref())
            .unwrap_or(0)
    }) % 4;
    let time = format!("+{:.1}s", event.seconds);
    let initial = actor
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    if event.kind == "message" {
        return view! {
            <li class=format!("sb-message sb-actor-{actor_index}")>
                <div class="sb-avatar" aria-hidden="true">{initial}</div>
                <div class="sb-message-body"><div class="sb-message-heading"><strong>{actor}</strong><span>{recipient}</span><time>{time}</time></div>
                    <p>{event.text}</p></div>
            </li>
        }.into_any();
    }
    let text = event.text;
    if matches!(event.kind.as_str(), "node_action" | "work_action") {
        return view! { <li class="sb-resource-row"><span class="sb-event-dot" aria-hidden="true"></span><p><strong>{actor}</strong>{" · "}{text}</p><time>{time}</time></li> }.into_any();
    }
    if event.kind == "finished" {
        return view! { <li class="sb-end-event"><span>"Observation ended"</span><time>{time}</time></li> }.into_any();
    }
    let kind = match event.kind.as_str() {
        "node_action" => "Resource",
        "intervention" => "Test injection",
        "scenario_check" => "Check",
        "finished" => "Run ended",
        "error" => "Error",
        "interrupted" => "Interrupted",
        "agent_message" => "Model output",
        "user_message" => "Task / wake prompt",
        "tool_call" => "Tool call",
        "turn_started" => "Turn started",
        "turn_completed" => "Turn completed",
        _ => "Event",
    };
    let detailed = matches!(
        event.kind.as_str(),
        "tool_call" | "user_message" | "turn_started" | "turn_completed"
    );
    if detailed {
        view! { <li class="sb-trace-event"><details><summary><span>{kind}</span><strong>{actor}</strong><time>{time}</time></summary><pre>{text}</pre></details></li> }.into_any()
    } else {
        view! { <li class="sb-resource-event" class:sb-error=event.kind == "error"><span class="sb-event-dot" aria-hidden="true"></span>
            <div><div class="sb-resource-heading"><span>{kind}</span><strong>{actor}</strong><time>{time}</time></div><p>{text}</p></div>
        </li> }.into_any()
    }
}

#[component]
fn Comparison(
    artifact: SandboxArtifact,
    runs: Memo<Vec<Arc<SandboxRun>>>,
    baseline: RwSignal<String>,
) -> impl IntoView {
    let a = StoredValue::new(artifact);
    let reference = Memo::new(move |_| {
        runs.get()
            .iter()
            .find(|run| {
                run.artifact.run_id == baseline.get()
                    && run.artifact.run_id != a.with_value(|a| a.run_id.clone())
            })
            .map(|run| run.artifact.clone())
    });
    view! {
        <div class="sb-comparison">
            <div class="sb-compare-heading">
                <label>"Baseline"<select aria-label="Zoo baseline" prop:value=move || reference.get().map(|a| a.run_id).unwrap_or_default()
                    on:change=move |ev| baseline.set(event_target_value(&ev))>
                    <option value="">"Select another run"</option>
                    {move || runs.get().iter().filter(|run| run.artifact.run_id != a.with_value(|a| a.run_id.clone())).map(|run| view! {
                        <option value=run.artifact.run_id.clone()>{format!("{} · {}", run.artifact.label, run.artifact.run_id.chars().take(8).collect::<String>())}</option>
                    }).collect_view()}
                </select></label>
            </div>
            {move || match reference.get() {
                None => view! { <Show when=move || runs.get().len() < 2><p class="sb-muted">"Import another run to compare."</p></Show> }.into_any(),
                Some(other) => {
                    let candidate = a.get_value();
                    let rows = metrics(&candidate).into_iter().zip(metrics(&other));
                    view! {
                        {(candidate.model != other.model).then(|| view! { <p class="sb-comparison-warning">"Model differs"</p> })}
                        {(candidate.scenario != other.scenario).then(|| view! { <p class="sb-comparison-warning">"Scenario differs"</p> })}
                        <div class="sb-table-scroll"><table class="sb-comparison-table">
                            <thead><tr><th>"Measure"</th><th>{format!("{} · selected",candidate.label)}</th><th>{format!("{} · baseline",other.label)}</th><th title="Selected run minus baseline">"Change"</th></tr></thead>
                            <tbody>{rows.map(|((label,value),(_,prior))| view! { <tr><th>{label}</th><td>{value.display()}</td><td>{prior.display()}</td><td class="sb-delta">{value.delta(prior)}</td></tr> }).collect_view()}</tbody>
                        </table></div>
                    }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn Context(artifact: SandboxArtifact) -> impl IntoView {
    view! {
        <div class="sb-context">
            <details open><summary>"Marshal instructions"</summary><pre>{artifact.instructions}</pre></details>
            <details><summary>"Tokens"</summary><dl><dt>"Input, incl. cached"</dt><dd>{count(artifact.input_tokens)}</dd><dt>"Cached"</dt><dd>{count(artifact.cached_input_tokens)}</dd><dt>"Output"</dt><dd>{count(artifact.output_tokens)}</dd></dl></details>
            <details><summary>"Runtime and build"</summary><dl><dt>"Run ID"</dt><dd>{artifact.run_id}</dd><dt>"Codex"</dt><dd>{artifact.codex_version}</dd>
                {artifact.binaries.into_iter().map(|(path,hash)| view! { <dt>{path}</dt><dd>{hash}</dd> }).collect_view()}</dl></details>
            <details><summary>"Agent identities"</summary><dl>{artifact.roles.into_iter().map(|(role,id)| view! { <dt>{role}</dt><dd>{id}</dd> }).collect_view()}</dl></details>
        </div>
    }
}

#[derive(Clone, Copy)]
enum MetricValue {
    Check(bool),
    Count(u64),
}
impl MetricValue {
    fn display(self) -> String {
        match self {
            Self::Check(value) => if value { "Yes" } else { "No" }.into(),
            Self::Count(value) => count(value),
        }
    }
    fn delta(self, other: Self) -> String {
        match (self, other) {
            (Self::Count(value), Self::Count(prior)) => {
                let difference = i128::from(value) - i128::from(prior);
                if difference == 0 {
                    "—".into()
                } else {
                    format!(
                        "{}{value}",
                        if difference > 0 { "+" } else { "-" },
                        value = count(difference.unsigned_abs() as u64)
                    )
                }
            }
            (Self::Check(value), Self::Check(prior)) => {
                if value == prior { "Same" } else { "Changed" }.into()
            }
            _ => unreachable!("comparison uses the same metric definitions"),
        }
    }
}
fn metrics(a: &SandboxArtifact) -> Vec<(&'static str, MetricValue)> {
    use MetricValue::{Check, Count};
    vec![
        ("Test completed", Check(a.tested)),
        ("Node released", Check(a.released)),
        ("Resource conflicts", Count(a.resource_violations.into())),
        ("Failed model turns", Count(a.failed_turns.into())),
        ("Messages", Count(a.messages.into())),
        ("Turns", Count(a.turns.into())),
        ("Wake turns", Count(a.wake_turns.into())),
        ("Input tokens, incl. cached", Count(a.input_tokens)),
        ("Cached input tokens", Count(a.cached_input_tokens)),
        ("Output tokens", Count(a.output_tokens)),
    ]
}
fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut parts = Vec::new();
    let mut end = digits.len();
    while end > 3 {
        parts.push(&digits[end - 3..end]);
        end -= 3;
    }
    parts.push(&digits[..end]);
    parts.reverse();
    parts.join(",")
}

const REVIEW_CSS: &str = r#"
.sb-review h3,.sb-review h4 { margin:0; }
.sb-run-header { display:flex; justify-content:space-between; align-items:flex-start; gap:16px; }
.sb-run-header h3 { font-size:25px; font-weight:600; letter-spacing:-.03em; overflow-wrap:anywhere; }
.sb-run-meta { display:flex; flex-wrap:wrap; gap:10px; color:var(--sb-muted); font:11px var(--font-mono); margin:10px 0 0; }
.sb-run-meta>span { opacity:.4; }
.sb-outcome { display:flex; align-items:center; gap:6px; border:1px solid var(--color-border); padding:5px 9px; border-radius:20px; font-size:11px; white-space:nowrap; }
.sb-outcome i { width:5px; height:5px; background:currentColor; border-radius:50%; }
.sb-stats { margin:16px 0; display:grid; grid-template-columns:repeat(4,minmax(0,1fr)); border:1px solid var(--color-border); border-radius:7px; background:var(--color-base-200); }
.sb-stats>div { padding:14px 16px; border-right:1px solid var(--color-border); }
.sb-stats>div:last-child { border-right:0; }
.sb-stats span { display:block; font-size:12px; color:var(--color-text-primary); margin-bottom:9px; }
.sb-stats strong { font:23px var(--font-mono); font-weight:500; font-variant-numeric:tabular-nums; }
.sb-tabs { position:sticky; top:0; z-index:2; background:var(--color-base-100); padding-top:8px; display:flex; gap:24px; border-bottom:1px solid var(--color-border); margin-bottom:20px; }
.sb-tabs>button { appearance:none; border:0; border-bottom:2px solid transparent; background:transparent; color:var(--sb-muted); font-size:12px; padding:0 0 12px; cursor:pointer; }
.sb-tabs>button:hover { color:var(--color-text-primary); }
.sb-tabs>button.sb-tab-active { color:var(--color-text-primary); border-bottom-color:var(--color-primary); }
.sb-review select { height:34px; padding:5px 8px; line-height:20px; font-size:11px; color:inherit; background:var(--color-base-100); border:1px solid var(--color-border); border-radius:5px; max-width:100%; }
.sb-timeline-controls { position:sticky; top:43px; z-index:1; padding:8px 0; background:var(--color-base-100); display:flex; align-items:center; gap:10px; }
.sb-timeline-controls>.sb-search { flex:1; min-width:100px; }
.sb-trace-toggle { display:flex; align-items:center; gap:6px; font-size:12px; color:var(--color-text-primary); white-space:nowrap; }
.sb-trace-toggle input { accent-color:var(--color-primary); }
.sb-transcript { list-style:none; margin:16px 0 0; padding:0; }
.sb-message { display:flex; align-items:flex-start; gap:12px; padding:18px 0; }
.sb-avatar { flex:0 0 29px; width:29px; height:29px; border:1px solid var(--color-border-hover); border-radius:7px; display:grid; place-items:center; font:11px var(--font-mono); background:var(--color-base-200); color:var(--color-primary); }
.sb-actor-1 .sb-avatar { color:#c6aff3; } .sb-actor-2 .sb-avatar { color:#e4b781; } .sb-actor-3 .sb-avatar { color:#9bc7f7; }
.sb-message-body { flex:1; min-width:0; border:1px solid var(--color-border); border-radius:8px; padding:13px 16px; background:var(--color-base-200); }
.sb-message-heading { display:flex; flex-wrap:wrap; align-items:center; gap:9px; font-size:11px; }
.sb-message-heading strong { font-weight:600; }
.sb-message-heading>span { color:var(--sb-muted); }
.sb-message-heading time { margin-left:auto; color:var(--sb-muted); font:10px var(--font-mono); }
.sb-message-body p { font-size:13px; line-height:1.75; white-space:pre-wrap; overflow-wrap:anywhere; margin:10px 0 0; }
.sb-issues { margin:12px 0; color:var(--color-warning); font-size:12px; }
.sb-resource-row { display:grid; grid-template-columns:6px minmax(0,1fr) auto; gap:12px; align-items:baseline; padding:10px 0 10px 12px; }
.sb-resource-row p { margin:0; color:var(--color-text-primary); font-size:12px; line-height:1.6; overflow-wrap:anywhere; }
.sb-resource-row strong { font-weight:500; }
.sb-resource-row time,.sb-end-event time { color:var(--sb-muted); font:10px var(--font-mono); }
.sb-end-event { display:flex; justify-content:space-between; margin:16px 0 0 41px; padding-top:12px; border-top:1px solid var(--color-border); font-size:11px; }
.sb-resource-event { display:flex; gap:22px; align-items:flex-start; padding:12px 0 12px 10px; margin-left:3px; }
.sb-event-dot { width:5px; height:5px; flex:0 0 5px; border-radius:50%; background:var(--color-border-hover); margin-top:5px; }
.sb-resource-event>div { flex:1; min-width:0; }
.sb-resource-heading { display:flex; flex-wrap:wrap; align-items:center; gap:9px; color:var(--sb-muted); font-size:10px; }
.sb-resource-heading strong { font-weight:400; }
.sb-resource-heading time { margin-left:auto; font-family:var(--font-mono); }
.sb-resource-event p { font-size:12px; line-height:1.6; color:var(--sb-muted); margin:6px 0 0; white-space:pre-wrap; overflow-wrap:anywhere; }
.sb-trace-event { margin:8px 0 8px 41px; border-left:1px solid var(--color-border); padding-left:12px; }
.sb-trace-event summary { cursor:pointer; display:flex; flex-wrap:wrap; gap:8px; color:var(--sb-muted); font-size:10px; padding:6px 0; }
.sb-trace-event summary::before { content:'▸'; } .sb-trace-event details[open]>summary::before { content:'▾'; }
.sb-trace-event time { margin-left:auto; font-family:var(--font-mono); }
.sb-review pre { font:11px/1.7 var(--font-mono); white-space:pre-wrap; overflow-wrap:anywhere; }
.sb-no-events { padding:40px 20px; text-align:center; border:1px dashed var(--color-border); border-radius:7px; }
.sb-no-events h4 { font-size:14px; }
.sb-no-events button { margin-top:14px; background:transparent; border:0; color:var(--color-primary); font-size:12px; cursor:pointer; }
.sb-compare-heading { display:flex; justify-content:space-between; align-items:flex-start; gap:16px; margin-bottom:20px; }
.sb-compare-heading label { display:flex; flex-direction:column; gap:7px; font-size:11px; min-width:170px; }
.sb-comparison-warning { color:var(--color-warning); font-size:12px; padding:10px; border:1px solid var(--color-border); border-radius:5px; }
.sb-table-scroll { overflow-x:auto; }
.sb-comparison-table { width:100%; border-collapse:collapse; font-size:12px; font-variant-numeric:tabular-nums; }
.sb-comparison-table th,.sb-comparison-table td { padding:11px 10px; border-bottom:1px solid var(--color-border); text-align:right; }
.sb-comparison-table th:first-child { text-align:left; font-weight:400; color:var(--sb-muted); }
.sb-comparison-table thead th { font-size:10px; color:var(--sb-muted); font-weight:400; }
.sb-delta { font:10px var(--font-mono); color:var(--sb-muted); }
.sb-context details { margin-bottom:12px; border:1px solid var(--color-border); border-radius:6px; padding:12px 14px; }
.sb-context summary { font-size:12px; cursor:pointer; }
.sb-context pre { margin:14px 0 0; padding-top:14px; border-top:1px solid var(--color-border); max-height:520px; overflow-y:auto; }
.sb-context dl { display:grid; grid-template-columns:minmax(90px,160px) minmax(0,1fr); gap:12px; margin-top:20px; font-size:11px; }
.sb-context dt { color:var(--sb-muted); overflow-wrap:anywhere; } .sb-context dd { margin:0; font-family:var(--font-mono); overflow-wrap:anywhere; }
@container sandbox (max-width:850px) { .sb-stats>div { padding:12px 10px; } .sb-stats strong { font-size:20px; } .sb-timeline-controls { flex-wrap:wrap; } .sb-trace-toggle { margin-left:auto; } .sb-compare-heading { flex-direction:column; } }
@container sandbox (max-width:500px) { .sb-run-header h3 { font-size:21px; } .sb-stats { grid-template-columns:repeat(2,1fr); } .sb-stats>div:nth-child(2) { border-right:0; } .sb-stats>div:nth-child(-n+2) { border-bottom:1px solid var(--color-border); } .sb-message { gap:8px; } .sb-avatar { flex-basis:24px; width:24px; height:24px; } .sb-message-body { padding:11px; } .sb-trace-event { margin-left:32px; } }
"#;
