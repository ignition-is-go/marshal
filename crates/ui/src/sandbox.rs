//! Shared experiment history, imported from isolated sandbox run artifacts.

use leptos::prelude::*;
use marshal_entities::{
    GetAllSandboxRuns, ImportSandboxRun, MAX_SANDBOX_ARTIFACT_BYTES, SandboxArtifact, SandboxRunId,
    SandboxStatus,
};
use pulse_leptos_ui::EmptyState;
use wasm_bindgen_futures::JsFuture;

#[component]
pub fn SandboxPanel() -> impl IntoView {
    let (runs, loaded) =
        myko_leptos::live_query_loaded::<GetAllSandboxRuns>(|| GetAllSandboxRuns {});
    let sink = myko_leptos::use_command_sink();
    let connected = myko_leptos::use_connection_status();
    let selected = RwSignal::new(String::new());
    let baseline = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let note = RwSignal::new(String::new());
    let all_runs = Memo::new(move |_| {
        let mut values = runs.get();
        values.sort_by_key(|run| std::cmp::Reverse(run.imported_at));
        values
    });
    let current = Memo::new(move |_| {
        let values = all_runs.get();
        values
            .iter()
            .find(|run| run.id.0.as_ref() == selected.get())
            .or_else(|| values.first())
            .map(|run| run.artifact.clone())
    });

    let import = move |event: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&event);
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        if file.size() > MAX_SANDBOX_ARTIFACT_BYTES as f64 {
            note.set("Choose a sandbox-run.json file under 2 MiB.".into());
            input.set_value("");
            return;
        }
        pending.set(true);
        note.set("Importing run…".into());
        let sink = sink.clone();
        leptos::task::spawn_local(async move {
            let parsed = async {
                let text = JsFuture::from(file.text())
                    .await
                    .map_err(|_| "Could not read the selected file".to_string())?
                    .as_string()
                    .ok_or_else(|| "The selected file is not text".to_string())?;
                let artifact: SandboxArtifact = serde_json::from_str(&text)
                    .map_err(|error| format!("Invalid run artifact: {error}"))?;
                artifact.validate()?;
                Ok::<_, String>(artifact)
            }
            .await;
            match parsed {
                Ok(artifact) => sink.dispatch::<ImportSandboxRun, SandboxRunId>(
                    ImportSandboxRun { artifact },
                    move |result| {
                        pending.set(false);
                        match result {
                            Ok(id) => {
                                selected.set(id.0.to_string());
                                note.set("Run saved to shared sandbox history.".into());
                            }
                            Err(error) => note.set(format!("Import failed: {error}")),
                        }
                    },
                ),
                Err(error) => {
                    pending.set(false);
                    note.set(error);
                }
            }
            input.set_value("");
        });
    };

    view! {
        <section class="sandbox-panel">
            <div class="panel-head"><h2>"Sandbox"</h2></div>
            <p class="sandbox-help">"Observe isolated agent experiments. Import sandbox-run.json to share a run and compare its outcomes."</p>
            <label class="sandbox-import">
                "Import run "
                <input type="file" accept=".json,application/json" aria-label="Import sandbox run"
                    disabled=move || pending.get() || !connected.get()
                    on:change=import />
            </label>
            <p role="status">{move || note.get()}</p>
            <Show when=move || !connected.get()>
                <p class="sandbox-help">"Connect to the daemon to load or import runs."</p>
            </Show>
            {move || {
                let values = all_runs.get();
                if values.is_empty() {
                    return view! { <EmptyState message=if loaded.get() {
                        "No sandbox runs imported yet. Run an experiment, then import its sandbox-run.json file.".to_string()
                    } else { "Loading sandbox runs…".to_string() } /> }.into_any();
                }
                view! {
                    <div class="sandbox-pickers">
                        <label>"Run"
                            <select aria-label="Sandbox run" prop:value=move || current.get().map(|a| a.run_id).unwrap_or_default()
                                on:change=move |ev| selected.set(event_target_value(&ev))>
                                {values.iter().map(|run| view! { <option value=run.id.0.to_string()>{format!("{} · {} · {}", run.artifact.label, run.artifact.model, run.artifact.run_id.chars().take(8).collect::<String>())}</option> }).collect_view()}
                            </select>
                        </label>
                        <label>"Compare with"
                            <select aria-label="Sandbox baseline" prop:value=move || baseline.get()
                                on:change=move |ev| baseline.set(event_target_value(&ev))>
                                <option value="">"No baseline"</option>
                                {values.iter().map(|run| view! { <option value=run.id.0.to_string()>{format!("{} · {} · {}", run.artifact.label, run.artifact.model, run.artifact.run_id.chars().take(8).collect::<String>())}</option> }).collect_view()}
                            </select>
                        </label>
                    </div>
                }.into_any()
            }}
            {move || current.get().map(|artifact| {
                let reference = all_runs.get().iter().find(|run| run.id.0.as_ref() == baseline.get()).map(|run| run.artifact.clone());
                view! {
                    <RunDetails artifact=artifact reference=reference />
                }
            })}
        </section>
        <style>{SANDBOX_CSS}</style>
    }
}

#[component]
fn RunDetails(artifact: SandboxArtifact, reference: Option<SandboxArtifact>) -> impl IntoView {
    let show_all = RwSignal::new(false);
    let outcome = if artifact.succeeded() {
        "Test completed and node released without conflicts"
    } else {
        "Run has not met all scenario success conditions"
    };
    let measures = metrics(&artifact);
    let comparison = reference.as_ref().map(|value| {
        metrics(value)
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>()
    });
    let mismatch = reference
        .as_ref()
        .is_some_and(|other| other.model != artifact.model || other.scenario != artifact.scenario);
    let timeline = artifact.timeline.clone();
    let context = artifact.instructions.clone();
    let binaries = artifact
        .binaries
        .iter()
        .map(|(path, hash)| format!("{path}\n{hash}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let run_label = format!(
        "{} · {}",
        artifact.label,
        artifact.run_id.chars().take(8).collect::<String>()
    );
    let baseline_label = reference
        .as_ref()
        .map(|value| {
            format!(
                "{} · {}",
                value.label,
                value.run_id.chars().take(8).collect::<String>()
            )
        })
        .unwrap_or_default();
    view! {
        <div class="sandbox-summary">
            <h3>{artifact.label.clone()}</h3>
            <p>{format!("{} · {} · {}", artifact.scenario, artifact.model, status_label(artifact.status))}</p>
            <p class:run-succeeded=artifact.succeeded()>{outcome}</p>
            {mismatch.then(|| view! { <p class="sandbox-help">"These runs use different models or scenarios. Their metrics are not a controlled comparison."</p> })}
            <div class="sandbox-table-scroll"><table class="sandbox-metrics">
                <thead><tr><th>"Measure"</th><th>{run_label}</th>{reference.as_ref().map(|_| view! { <th>{baseline_label}</th> })}</tr></thead>
                <tbody>{measures.into_iter().enumerate().map(|(index, (label, value))| {
                    let other = comparison.as_ref().map(|values| values[index].clone());
                    view! { <tr><th>{label}</th><td>{value}</td>{other.map(|value| view! { <td>{value}</td> })}</tr> }
                }).collect_view()}</tbody>
            </table></div>
            <p class="sandbox-help">"Cached input tokens are included in the input total."</p>
            <details><summary>"Instructions and provenance"</summary>
                <p>{format!("Run {} · {}", artifact.run_id, artifact.codex_version)}</p>
                <pre>{binaries}</pre><h4>"Generated agent instructions"</h4><pre>{context}</pre>
            </details>
            <div class="sandbox-timeline-head"><h3>"Timeline"</h3>
                <label><input type="checkbox" prop:checked=move || show_all.get()
                    on:change=move |ev| show_all.set(event_target_checked(&ev)) />" Include model turns and tools"</label>
            </div>
            <ol class="sandbox-timeline">
                {timeline.into_iter().map(|event| {
                    let detailed = matches!(event.kind.as_str(), "agent_message" | "user_message" | "tool_call" | "turn_started" | "turn_completed");
                    let actor = event.actor.as_deref().map(|id| artifact.actor_name(id)).unwrap_or_else(|| "runner".into());
                    let recipient = event.recipient.as_deref().map(|id| format!(" → {}", artifact.actor_name(id))).unwrap_or_default();
                    view! { <li style:display=move || if detailed && !show_all.get() { "none" } else { "block" }>
                        <div class="sandbox-event-meta"><time>{format!("{:.1}s", event.seconds)}</time><span>{event.kind.replace('_', " ")}</span><strong>{actor}{recipient}</strong></div>
                        <pre>{event.text}</pre>
                    </li> }
                }).collect_view()}
            </ol>
        </div>
    }
}

fn metrics(artifact: &SandboxArtifact) -> Vec<(&'static str, String)> {
    vec![
        ("Test completed", yes_no(artifact.tested)),
        ("Node released", yes_no(artifact.released)),
        (
            "Resource conflicts",
            artifact.resource_violations.to_string(),
        ),
        ("Failed model turns", artifact.failed_turns.to_string()),
        ("Messages", artifact.messages.to_string()),
        ("Turns", artifact.turns.to_string()),
        ("Wake turns", artifact.wake_turns.to_string()),
        ("Input tokens", artifact.input_tokens.to_string()),
        (
            "Cached input tokens",
            artifact.cached_input_tokens.to_string(),
        ),
        ("Output tokens", artifact.output_tokens.to_string()),
    ]
}

fn yes_no(value: bool) -> String {
    if value { "Yes" } else { "No" }.into()
}

fn status_label(status: SandboxStatus) -> &'static str {
    match status {
        SandboxStatus::Starting => "Starting",
        SandboxStatus::Running => "Running",
        SandboxStatus::Quiescent => "Quiescent",
        SandboxStatus::BudgetExhausted => "Budget exhausted",
        SandboxStatus::Deadline => "Deadline reached",
        SandboxStatus::ModelTurnFailed => "Model turn failed",
        SandboxStatus::Interrupted => "Interrupted",
        SandboxStatus::Error => "Runtime error",
    }
}

const SANDBOX_CSS: &str = r#"
.sandbox-panel { display:flex; flex-direction:column; gap:12px; }
.sandbox-help { color:var(--color-text-secondary); font-size:13px; }
.sandbox-import { padding:12px; border:1px dashed var(--color-border); border-radius:var(--radius-sm); }
.sandbox-pickers { display:flex; flex-wrap:wrap; gap:12px; }
.sandbox-pickers label { flex:1; min-width:180px; display:flex; flex-direction:column; gap:6px; }
.sandbox-pickers select { width:100%; height:36px; line-height:20px; padding:4px 8px; background:var(--color-base-200); color:inherit; border:1px solid var(--color-border); border-radius:var(--radius-sm); }
.sandbox-summary { display:flex; flex-direction:column; gap:12px; }
.run-succeeded { color:var(--color-success); }
.sandbox-table-scroll { overflow-x:auto; }
.sandbox-metrics { width:100%; border-collapse:collapse; font-variant-numeric:tabular-nums; }
.sandbox-metrics th,.sandbox-metrics td { text-align:left; padding:7px 10px; border-bottom:1px solid var(--color-border); }
.sandbox-metrics td { text-align:right; }
.sandbox-summary pre { white-space:pre-wrap; overflow-wrap:anywhere; font-size:12px; margin:6px 0; }
.sandbox-summary details { padding:10px; background:var(--color-base-200); border-radius:var(--radius-sm); }
.sandbox-summary summary { cursor:pointer; }
.sandbox-timeline-head { display:flex; align-items:center; flex-wrap:wrap; justify-content:space-between; gap:8px; }
.sandbox-timeline-head label { font-size:12px; }
.sandbox-timeline { list-style:none; margin:0; padding:0; }
.sandbox-timeline li { padding:12px; margin-bottom:8px; border:1px solid var(--color-border); border-radius:var(--radius-sm); }
.sandbox-event-meta { display:flex; flex-wrap:wrap; gap:10px; font-size:12px; color:var(--color-text-secondary); }
.sandbox-event-meta strong { color:var(--color-text-primary); }
"#;
