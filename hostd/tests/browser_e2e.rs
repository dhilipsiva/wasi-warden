//! End to end through the browser path: a scripted plan opens a WebDriver session and navigates it,
//! against a fake W3C WebDriver. The host used to panic here (`Handle::block_on` inside the async
//! runtime, exit 101) on the first browser action.
//!
//! Needs the agent component (see `step_e2e.rs`), so it is ignored by default:
//! `cargo test -p hostd -- --include-ignored`.

mod support;

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_browser_session_opens_navigates_and_quits() {
    let root = support::scratch_dir("browser");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let target = "http://login.example.test/";
    let (llm_port, llm_seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "open the login page",
            "actions": [
                { "capability": "browser.open_session", "input": { "alias": "s1" } },
                {
                    "capability": "browser.session.goto",
                    "input": { "session": "s1", "url": target, "timeout_ms": 0 }
                }
            ]
        }),
        serde_json::json!({
            "status": "complete",
            "thought": "on the page",
            "actions": [],
            "result": { "opened": true },
            "reason": "the login page is open"
        }),
    ]);
    let (wd_port, wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));

    let out = support::run_step(&config, "Open the login page");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "hostd failed ({:?}):\n{stderr}",
        out.status.code()
    );

    let wd = wd_seen.lock().expect("webdriver log");
    let session = format!("/session/{}", support::SESSION);
    assert!(
        wd.iter().any(|(m, p, _)| m == "POST" && p == "/session"),
        "a session was created: {wd:?}"
    );
    assert!(
        wd.iter()
            .any(|(m, p, b)| m == "POST" && *p == format!("{session}/url") && b.contains(target)),
        "the session was sent to {target}: {wd:?}"
    );
    assert!(
        wd.iter().any(|(m, p, _)| m == "DELETE" && *p == session),
        "the session was quit when the step ended: {wd:?}"
    );
    let llm = llm_seen.lock().expect("llm log");
    assert_eq!(llm.len(), 2, "two planner rounds");
    let reported = llm[1].contains(target) && llm[1].contains("browser.session.goto");
    assert!(reported, "the second round saw the navigation: {}", llm[1]);
    let _ = std::fs::remove_dir_all(&root);
}
