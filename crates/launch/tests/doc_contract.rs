//! The JSON examples in docs/agent-launcher.md parse as this crate's types, so
//! the doc and the code can't drift apart. The doc lives outside the crate, so
//! this test runs in the workspace only and stays out of the published package.

use marshal_launch::{LaunchRequest, Launched, PROTOCOL_VERSION};

const DOC: &str = include_str!("../../../docs/agent-launcher.md");

/// The first ```json block after `marker`.
fn json_after(marker: &str) -> serde_json::Value {
    let at = DOC
        .find(marker)
        .unwrap_or_else(|| panic!("{marker:?} isn't in the doc"));
    let rest = &DOC[at..];
    let start = rest.find("```json\n").expect("a json block") + "```json\n".len();
    let body = &rest[start..];
    let end = body.find("```").expect("the block's end");
    serde_json::from_str(&body[..end]).unwrap_or_else(|e| panic!("doc json after {marker:?}: {e}"))
}

#[test]
fn the_docs_launch_request_parses() {
    let v = json_after("**`launch`** — request:");
    let req: LaunchRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(req.version, PROTOCOL_VERSION);
    assert_eq!(req.harness, "claude");
    assert_eq!(req.argv, ["claude"]);
    assert_eq!(req.options, ["--host", "build-03"]);
    assert_eq!(req.env.get("MARSHAL_AGENT_ID"), Some(&req.agent_id));
    assert_eq!(req.marshal_version, "0.60.2");
    // Nothing in the doc's example is dropped on the way through.
    assert_eq!(serde_json::to_value(&req).unwrap(), v);
}

#[test]
fn the_docs_launch_response_parses() {
    let v = json_after("Response:");
    assert_eq!(v["version"], PROTOCOL_VERSION);
    let l: Launched = serde_json::from_value(v).unwrap();
    assert_eq!(l.host, "build-03");
    assert_eq!(l.cwd, "/home/agent/work/app");
    assert_eq!(l.r#where, "build-03 tmux:claude-app");
    assert_eq!(
        l.attach.as_deref(),
        Some("ssh -t build-03 tmux attach -t '=claude-app'")
    );
}
