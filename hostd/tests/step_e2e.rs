//! End to end: `hostd step` loads the real agent component, which plans through a scripted
//! OpenAI-compatible endpoint; the host runs the planned `fs.list_dir` in a temporary workspace and
//! feeds the listing back, and the second plan completes the task.
//!
//! Needs the component: `cargo build -p agent-core --target wasm32-wasip2 --release` (or
//! `AGENT_CORE_WASM=<path>`), so it is ignored by default:
//! `cargo test -p hostd -- --include-ignored`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

/// The agent component: `AGENT_CORE_WASM`, or the release build in this test's target directory.
fn component() -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_CORE_WASM") {
        return PathBuf::from(path);
    }
    // <target>/debug/deps/step_e2e-<hash> -> <target>
    let exe = std::env::current_exe().expect("current exe");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("the target directory above debug/deps");
    target.join("wasm32-wasip2/release/agent_core.wasm")
}

/// A fresh directory under the system temp directory.
fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("hostd-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A chat-completions endpoint that answers each request with the next of `plans` (the last one
/// repeats) and records each request body.
fn fake_llm(plans: Vec<serde_json::Value>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).expect("request body");
            let n = {
                let mut seen = log.lock().expect("log");
                seen.push(String::from_utf8_lossy(&body).into_owned());
                seen.len()
            };
            let plan = &plans[(n - 1).min(plans.len() - 1)];
            let reply = serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": plan.to_string() },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (port, seen)
}

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_step_lists_the_workspace_and_completes() {
    let component = component();
    assert!(
        component.exists(),
        "{} is missing: cargo build -p agent-core --target wasm32-wasip2 --release",
        component.display()
    );
    let root = scratch_dir("e2e");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("planted-by-the-test.txt"), "hello").expect("plant a file");

    let (port, seen) = fake_llm(vec![
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
    std::fs::write(
        &config,
        format!(
            "workspace_root = {:?}\nallow_proc = []\n\n[llm]\napi_base = \"http://127.0.0.1:{port}/v1\"\napi_key = \"test\"\nmodel = \"scripted\"\n",
            workspace.display().to_string()
        ),
    )
    .expect("config");

    let out = Command::new(env!("CARGO_BIN_EXE_hostd"))
        .arg("step")
        .arg("--component")
        .arg(&component)
        .arg("--config")
        .arg(&config)
        .args(["--task", "List workspace files", "--observation", "{}"])
        .env("RUST_LOG", "info")
        .output()
        .expect("run hostd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "hostd failed:\n{stdout}\n{stderr}");

    let seen = seen.lock().expect("log");
    assert_eq!(seen.len(), 2, "two planner rounds: {seen:?}");
    assert!(
        seen[1].contains("planted-by-the-test.txt"),
        "the second round saw the listing: {}",
        seen[1]
    );
    let _ = std::fs::remove_dir_all(&root);
}
