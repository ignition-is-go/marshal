//! Experiment library and import controls. Run review stays separate from live-agent focus.

use leptos::prelude::*;
use marshal_entities::{
    GetAllSandboxRuns, ImportSandboxRun, MAX_SANDBOX_ARTIFACT_BYTES, SandboxArtifact, SandboxRunId,
};
use wasm_bindgen_futures::JsFuture;

use crate::sandbox_run::{ReviewTab, RunReview, outcome};

#[component]
pub fn SandboxPanel() -> impl IntoView {
    let (runs, loaded) =
        myko_leptos::live_query_loaded::<GetAllSandboxRuns>(|| GetAllSandboxRuns {});
    let sink = myko_leptos::use_command_sink();
    let connected = myko_leptos::use_connection_status();
    let selected = RwSignal::new(String::new());
    let baseline = RwSignal::new(String::new());
    let tab = RwSignal::new(ReviewTab::Conversation);
    let search = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let note = RwSignal::new(String::new());
    let failed_import = RwSignal::new(false);
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
            .cloned()
    });
    let visible = Memo::new(move |_| {
        let query = search.get().to_lowercase();
        all_runs
            .get()
            .into_iter()
            .filter(|run| {
                let a = &run.artifact;
                format!("{} {} {} {}", a.label, a.model, a.scenario, a.run_id)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect::<Vec<_>>()
    });
    let import = move |event: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&event);
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        if file.size() > MAX_SANDBOX_ARTIFACT_BYTES as f64 {
            failed_import.set(true);
            note.set("Choose a sandbox-run.json file under 2 MiB.".into());
            input.set_value("");
            return;
        }
        pending.set(true);
        failed_import.set(false);
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
                                if baseline.get_untracked() == id.0.as_ref() {
                                    baseline.set(String::new());
                                }
                                selected.set(id.0.to_string());
                                search.set(String::new());
                                note.set("Run saved to shared Zoo history.".into());
                            }
                            Err(error) => {
                                failed_import.set(true);
                                note.set(format!("Import failed: {error}"));
                            }
                        }
                    },
                ),
                Err(error) => {
                    pending.set(false);
                    failed_import.set(true);
                    note.set(error);
                }
            }
            input.set_value("");
        });
    };

    view! {
        <section class="sandbox-panel">
            <header class="sb-header">
                <div><div class="sb-eyebrow">"AGENT BEHAVIOR / EXPERIMENTS"</div><h2>"Marshal Zoo"</h2>
                    <p>"Review how agents coordinate, use shared resources, and stop."</p></div>
                <label class="sb-import" class:sb-disabled=move || pending.get() || !connected.get()>
                    <span aria-hidden="true">"+"</span> {move || if pending.get() { "Importing…" } else { "Import run" }}
                    <input class="sb-file-input" type="file" accept=".json,application/json" aria-label="Import Zoo run"
                        disabled=move || pending.get() || !connected.get() on:change=import />
                </label>
            </header>
            <Show when=move || !note.get().is_empty()>
                <p class="sb-notice" class:sb-error=move || failed_import.get() role="status">{move || note.get()}</p>
            </Show>
            <Show when=move || !connected.get()>
                <p class="sb-notice sb-error">"Disconnected. Reconnect to load or import shared runs."</p>
            </Show>
            <div class="sb-workspace">
                <aside class="sb-library" aria-label="Experiment library">
                    <div class="sb-library-heading"><h3>"Runs"</h3><span>{move || all_runs.get().len()}</span></div>
                    <input class="sb-search" type="search" placeholder="Find a run…" aria-label="Search Zoo runs"
                        prop:value=move || search.get() on:input=move |ev| search.set(event_target_value(&ev)) />
                    <nav class="sb-run-list" aria-label="Zoo runs">
                        {move || visible.get().into_iter().map(|run| {
                            let a = &run.artifact;
                            let id = a.run_id.clone();
                            let selected_id = id.clone();
                            let pressed_id = id.clone();
                            let (label, tone) = outcome(a);
                            view! {
                                <button class="sb-run" data-run-id=id.clone()
                                    class:sb-run-selected=move || current.get().is_some_and(|r| r.artifact.run_id == selected_id)
                                    aria-pressed=move || current.get().is_some_and(|r| r.artifact.run_id == pressed_id).to_string()
                                    on:click=move |_| { if baseline.get_untracked() == id { baseline.set(String::new()); } selected.set(id.clone()); }>
                                    <span class="sb-run-name">{a.label.clone()}</span>
                                    <span class=format!("sb-run-state {tone}")><i></i>{label}</span>
                                    <span class="sb-run-model">{a.model.clone()}</span>
                                    <span class="sb-run-id">{a.run_id.chars().take(8).collect::<String>()}</span>
                                </button>
                            }
                        }).collect_view()}
                    </nav>
                    <Show when=move || visible.get().is_empty() && !all_runs.get().is_empty()>
                        <p class="sb-muted">"No matching runs."</p>
                    </Show>
                    <p class="sb-library-note">"Imported snapshots. Experiments run independently of live agents."</p>
                </aside>
                <main class="sb-stage">
                    {move || match current.get() {
                        Some(run) => view! { <RunReview artifact=run.artifact.clone() runs=all_runs baseline=baseline tab=tab /> }.into_any(),
                        None => view! {
                            <div class="sb-empty"><div class="sb-empty-mark" aria-hidden="true">"↔"</div>
                                <h3>{move || if loaded.get() { "Your first experiment starts here" } else { "Loading experiments…" }}</h3>
                                <p>"Import a sandbox-run.json file to review its conversation, resource actions, and outcome."</p>
                                <p class="sb-muted">"The run stays separate from the live roster and message feed."</p>
                            </div>
                        }.into_any(),
                    }}
                </main>
            </div>
        </section>
        <style>{SANDBOX_CSS}</style>
    }
}

const SANDBOX_CSS: &str = r#"
.sandbox-panel { container-type:inline-size; container-name:sandbox; --sb-muted:var(--color-text-secondary); max-width:1440px; margin:0 auto; padding:12px 10px 36px; color:var(--color-text-primary); }
.sb-header { display:flex; justify-content:space-between; align-items:center; gap:24px; padding:8px 0 24px; border-bottom:1px solid var(--color-border); }
.sb-eyebrow { color:var(--sb-muted); font:10px var(--font-mono); letter-spacing:.12em; margin-bottom:8px; }
.sb-header h2 { font-size:25px; font-weight:600; letter-spacing:-.03em; margin:0; }
.sb-header p { color:var(--sb-muted); font-size:13px; margin:8px 0 0; }
.sb-import { display:inline-flex; align-items:center; gap:8px; padding:9px 14px; border:1px solid var(--color-border-hover); background:var(--color-base-200); border-radius:6px; font-size:12px; font-weight:600; cursor:pointer; white-space:nowrap; }
.sb-import:hover { border-color:var(--color-primary); }
.sb-import:focus-within { outline:2px solid var(--color-primary); outline-offset:3px; }
.sb-file-input { position:absolute; width:1px; height:1px; padding:0; overflow:hidden; clip-path:inset(50%); }
.sb-disabled { opacity:.45; cursor:not-allowed; }
.sb-notice { margin:12px 0 0; font-size:12px; color:var(--color-primary); padding:8px 12px; background:var(--color-base-200); border-radius:5px; }
.sb-error { color:var(--color-error); }
.sb-workspace { display:grid; grid-template-columns:210px minmax(0,1fr); gap:32px; padding-top:24px; }
.sb-library { position:sticky; top:0; align-self:start; min-width:0; max-height:calc(100vh - 200px); overflow-y:auto; }
.sb-library-heading { display:flex; align-items:center; justify-content:space-between; margin-bottom:12px; }
.sb-library-heading h3 { font-size:12px; font-weight:600; margin:0; }
.sb-library-heading>span { font:10px var(--font-mono); color:var(--sb-muted); padding:2px 6px; border:1px solid var(--color-border); border-radius:4px; }
.sb-search { width:100%; height:34px; line-height:20px; padding:6px 9px; font-size:12px; color:inherit; background:var(--color-base-100); border:1px solid var(--color-border); border-radius:5px; }
.sb-search:focus { outline:1px solid var(--color-primary); }
.sb-run-list { display:flex; flex-direction:column; gap:6px; margin-top:12px; }
.sb-run { width:100%; display:grid; grid-template-columns:1fr auto; gap:7px 8px; text-align:left; padding:12px 10px; border:1px solid transparent; border-radius:6px; background:transparent; color:inherit; cursor:pointer; }
.sb-run:hover { background:var(--color-base-200); }
.sb-run-selected { border-color:var(--color-border-hover); background:var(--color-base-200); box-shadow:inset 2px 0 var(--color-primary); }
.sb-run-name { font-size:13px; font-weight:600; overflow:hidden; text-overflow:ellipsis; }
.sb-run-state { display:flex; align-items:center; gap:5px; font-size:10px; }
.sb-run-state i { width:5px; height:5px; border-radius:50%; background:currentColor; }
.sb-run-model,.sb-run-id { font:10px var(--font-mono); color:var(--sb-muted); overflow-wrap:anywhere; }
.sb-run-id { text-align:right; }
.sb-library-note { color:var(--sb-muted); font-size:11px; line-height:1.6; margin-top:24px; padding-top:14px; border-top:1px solid var(--color-border); }
.sb-stage { min-width:0; max-width:1080px; }
.sb-muted { color:var(--sb-muted); font-size:12px; line-height:1.6; }
.sb-empty { padding:80px 24px; text-align:center; border:1px dashed var(--color-border); border-radius:8px; }
.sb-empty-mark { font-size:30px; color:var(--color-primary); margin-bottom:20px; }
.sb-empty h3 { font-size:20px; margin:0 0 14px; }
.sb-empty p { max-width:440px; margin:10px auto; font-size:13px; line-height:1.7; }
.sb-good { color:var(--color-success); } .sb-bad { color:var(--color-error); } .sb-neutral { color:var(--sb-muted); }
@container sandbox (max-width:900px) { .sb-workspace { grid-template-columns:170px minmax(0,1fr); gap:20px; } .sb-run { grid-template-columns:1fr; } .sb-run-id { display:none; } }
@container sandbox (max-width:650px) { .sandbox-panel { padding:0 0 24px; } .sb-header { align-items:flex-start; flex-wrap:wrap; gap:12px; } .sb-header h2 { font-size:22px; } .sb-header p { max-width:240px; } .sb-workspace { display:block; padding-top:16px; } .sb-library { position:static; max-height:none; margin-bottom:24px; } .sb-run-list { flex-direction:row; overflow-x:auto; } .sb-run { min-width:170px; max-width:210px; } .sb-library-note { display:none; } }
"#;
