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

/// A process the host spawned dies with the host, even one killed outright (the process runs in a
/// group of its own, so a kill of the host's group would not reach it otherwise).
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_spawned_process_dies_with_the_host() {
    let root = support::scratch_dir("deathsig");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (port, _seen) = support::fake_llm(vec![serde_json::json!({
        "status": "continue",
        "thought": "a long command",
        "actions": [{
            "capability": "proc.spawn",
            "input": { "command": "sh", "args": ["-c", "echo $$ > child.pid; exec sleep 30"] }
        }]
    })]);
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, port, None);
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("allow_proc = []", "allow_proc = [\"sh\"]");
    std::fs::write(&config, text).unwrap();
    let mut host = std::process::Command::new(env!("CARGO_BIN_EXE_hostd"))
        .arg("step")
        .arg("--component")
        .arg(support::component())
        .arg("--config")
        .arg(&config)
        .args(["--task", "Run long", "--observation", "{}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run hostd");
    let pid_file = workspace.join("child.pid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    host.kill().expect("kill hostd");
    let _ = host.wait();
    let gone_by = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut gone = false;
    while std::time::Instant::now() < gone_by {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => {
                gone = true;
                break;
            }
            Ok(stat)
                if stat
                    .rsplit_once(')')
                    .is_some_and(|(_, r)| r.trim_start().starts_with('Z')) =>
            {
                gone = true;
                break;
            }
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    assert!(gone, "the child {pid} outlived the host");
    let _ = std::fs::remove_dir_all(&root);
}
