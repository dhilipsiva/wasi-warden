//! End to end through the WIT capabilities: the probe guest (tests/probe-agent) calls fs, proc and
//! WASI itself through the real bindings and writes what each call returned to `results.json`. This
//! covers the host code agent-core never reaches, the workspace and process rules on that path, and
//! guest calls that used to crash the host.
//!
//! Needs the probe component: `cargo build -p probe-agent --target wasm32-wasip2 --release` (or
//! `PROBE_AGENT_WASM=<path>`), so it is ignored by default.

mod support;

use std::path::Path;

/// The absolute path of `program` on PATH.
fn which(program: &str) -> String {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|p| p.is_file())
        })
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| panic!("{program} is not on PATH"))
}

fn executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn the_wit_capabilities_hold_their_rules() {
    let root = support::scratch_dir("probe");
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    std::fs::create_dir_all(workspace.join("planted")).expect("workspace");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(workspace.join("seed.txt"), "seed").expect("seed");
    std::fs::write(workspace.join("big.bin"), vec![7u8; 2 << 20]).expect("big file");
    std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
    std::os::unix::fs::symlink(&outside, workspace.join("escape")).expect("symlink");
    // A program named like an allowed one, planted in the workspace.
    let planted = workspace.join("planted/echo");
    executable(&planted, "#!/bin/sh\necho planted\n");

    let config = root.join("hostd.toml");
    std::fs::write(
        &config,
        format!(
            "workspace_root = {:?}\nallow_proc = [\"echo\", \"sleep\"]\n",
            workspace.display().to_string()
        ),
    )
    .expect("config");
    let task = serde_json::json!({
        "echo": "echo",
        "sleep": "sleep",
        "planted": planted.display().to_string(),
    })
    .to_string();
    // Sanity: the allowlist resolves through this PATH.
    let _ = (which("echo"), which("sleep"));

    let out = support::run_step_with(&support::probe_component(), &config, &task);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "hostd failed ({:?}):\n{stderr}",
        out.status.code()
    );
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.join("results.json")).expect("results.json"),
    )
    .expect("results are JSON");

    assert_eq!(results["wasi"], "ok", "WASI stdout, stderr and sleep work");
    let listed = results["list"].to_string();
    assert!(listed.contains("seed.txt"), "{listed}");
    assert_eq!(results["write_read"], "probe", "a written file reads back");
    assert_eq!(
        results["escape_dir"], "refused",
        "a symlinked directory out of the workspace"
    );
    assert_eq!(
        results["escape_file"], "refused",
        "a file through that symlink"
    );
    let huge = results["huge_read"].as_u64().unwrap();
    assert!(
        huge > 0 && huge <= 1 << 20,
        "an unbounded read is capped at 1 MiB: {huge}"
    );
    assert_eq!(results["spawn_echo"], "hi\n", "an allowed program runs");
    assert_eq!(results["spawn_ld"], "refused", "LD_PRELOAD from the guest");
    assert_eq!(
        results["spawn_planted"], "refused",
        "a planted program named like an allowed one"
    );
    let timeout = &results["spawn_timeout"];
    assert_eq!(
        timeout["timed_out"], true,
        "the timeout is honoured: {timeout}"
    );
    assert!(
        timeout["elapsed_ms"].as_u64().unwrap() < 5_000,
        "and the process is killed: {timeout}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
