//! launches.json round trips under the target's temp dir.

use marshal_launch::record::{Launch, Store};

fn launch(agent_id: &str, name: &str) -> Launch {
    Launch {
        adapter: "tmux".into(),
        handle: name.into(),
        agent_id: agent_id.into(),
        name: name.into(),
        harness: "claude".into(),
        host: "build-03".into(),
        cwd: "/w".into(),
        r#where: format!("build-03 tmux:{name}"),
        attach: Some(format!("tmux attach -t ={name}")),
        started_at: 1,
        daemon: None,
        session_id: None,
    }
}

#[test]
fn launches_are_added_marked_joined_and_removed() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("launches-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::at(dir.join("marshal").join("launches.json"));
    assert!(store.list().unwrap().is_empty(), "no file is no launches");
    store.add(launch("launch-1", "a")).unwrap();
    store.add(launch("launch-2", "b")).unwrap();
    store.joined("launch-2", "sess-2").unwrap();
    let ls = store.list().unwrap();
    assert_eq!(ls.len(), 2);
    assert_eq!(ls[1].session_id.as_deref(), Some("sess-2"));
    // Re-adding the same agent id replaces it.
    store.add(launch("launch-1", "a2")).unwrap();
    let ls = store.list().unwrap();
    assert_eq!(
        ls.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        ["b", "a2"]
    );
    store.remove("launch-2").unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(!dir.join("marshal").join("launches.json.lock").exists());
    // Many writers at once lose nothing.
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = Store::at(store.path());
            std::thread::spawn(move || store.add(launch(&format!("launch-t{i}"), "t")).unwrap())
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(store.list().unwrap().len(), 9);
    std::fs::remove_dir_all(&dir).unwrap();
}
