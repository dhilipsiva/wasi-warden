//! End to end: `hostd step` loads the real agent component, which plans through a scripted
//! OpenAI-compatible endpoint; the host runs the planned `fs.list_dir` in a temporary workspace and
//! feeds the listing back, and the second plan completes the task.
//!
//! Needs the component: `cargo build -p agent-core --target wasm32-wasip2 --release` (or
//! `AGENT_CORE_WASM=<path>`), so it is ignored by default:
//! `cargo test -p hostd -- --include-ignored`.

mod support;

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_step_lists_the_workspace_and_completes() {
    let root = support::scratch_dir("e2e");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("planted-by-the-test.txt"), "hello").expect("plant a file");

    let (port, seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "list the workspace",
            "actions": [{ "capability": "fs.list_dir", "input": { "path": "." } }]
        }),
        serde_json::json!({
            "status": "complete",
            "thought": "listed",
            "actions": [],
            "result": { "listed": true },
            "reason": "the workspace was listed"
        }),
    ]);
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, port, None);

    let out = support::run_step(&config, "List workspace files");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "hostd failed:\n{stderr}");

    let seen = seen.lock().expect("log");
    assert_eq!(seen.len(), 2, "two planner rounds: {seen:?}");
    assert!(
        seen[1].contains("planted-by-the-test.txt"),
        "the second round saw the listing: {}",
        seen[1]
    );
    let _ = std::fs::remove_dir_all(&root);
}
