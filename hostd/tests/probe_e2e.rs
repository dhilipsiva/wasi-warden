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
    std::fs::create_dir_all(outside.join("sub")).expect("outside/sub");
    // For the stale-handle swaps: a second link out, and a relative `..` link that stays inside.
    std::os::unix::fs::symlink(&outside, workspace.join("swap-out")).expect("symlink");
    std::fs::create_dir_all(workspace.join("a")).expect("a");
    std::os::unix::fs::symlink("..", workspace.join("a/up")).expect("relative symlink");
    // A program named like an allowed one, planted in the workspace.
    let planted = workspace.join("planted/echo");
    executable(&planted, "#!/bin/sh\necho planted\n");

    let config = root.join("hostd.toml");
    std::fs::write(
        &config,
        format!(
            "workspace_root = {:?}\nallow_proc = [\"echo\", \"sleep\", \"env\"]\n",
            workspace.display().to_string()
        ),
    )
    .expect("config");
    let task = serde_json::json!({
        "echo": "echo",
        "sleep": "sleep",
        "env": "env",
        "planted": planted.display().to_string(),
    })
    .to_string();
    // Sanity: the allowlist resolves through this PATH.
    let _ = (which("echo"), which("sleep"), which("env"));

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
    let seen_env = results["spawn_env"].as_str().unwrap_or_default();
    let names: Vec<&str> = seen_env
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    assert_eq!(names, ["PATH"], "only the host's PATH: {seen_env}");
    assert!(
        !seen_env.contains(&workspace.display().to_string()),
        "and none of it in the workspace: {seen_env}"
    );
    assert_eq!(results["spawn_ld"], "refused", "LD_PRELOAD from the guest");
    assert_eq!(
        results["spawn_planted"], "refused",
        "a planted program named like an allowed one"
    );
    for key in [
        "escape_ensure",
        "escape_metadata",
        "escape_remove_file",
        "escape_remove_dir",
        "escape_rename",
        "escape_rename_in",
        "escape_working_dir",
        "spawn_path",
        "stale_list",
        "stale_metadata",
        "stale_up_list",
    ] {
        assert_eq!(results[key], "refused", "{key}: {results}");
    }
    assert!(
        outside.join("secret.txt").exists(),
        "nothing outside was removed"
    );
    assert!(outside.join("sub").exists(), "nor any directory");
    assert!(
        !outside.join("stolen.txt").exists(),
        "and nothing was moved out"
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

/// A probe workspace and a config for it with `extra` sections; returns (root, workspace, config).
fn probe_setup(
    name: &str,
    extra: &str,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = support::scratch_dir(name);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("seed.txt"), "seed").expect("seed");
    std::fs::write(workspace.join("other.txt"), "other").expect("other");
    let config = root.join("hostd.toml");
    std::fs::write(
        &config,
        format!(
            "workspace_root = {:?}\n{extra}\n",
            workspace.display().to_string()
        ),
    )
    .expect("config");
    (root, workspace, config)
}

/// Run the probe in `mode` and read its results.
fn probe_results(
    config: &std::path::Path,
    workspace: &std::path::Path,
    task: serde_json::Value,
) -> serde_json::Value {
    let out = support::run_step_with(&support::probe_component(), config, &task.to_string());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "hostd failed ({:?}):\n{stderr}",
        out.status.code()
    );
    serde_json::from_str(&std::fs::read_to_string(workspace.join("results.json")).expect("results"))
        .expect("results are JSON")
}

/// A guest that computes forever is stopped when its fuel runs out.
#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn a_guest_that_burns_fuel_is_interrupted() {
    let (root, _ws, config) = probe_setup(
        "fuel",
        "[budgets]\nguest_fuel = 50000000\nwallclock_ms = 60000\n",
    );
    let out = support::run_step_within(
        &support::probe_component(),
        &config,
        r#"{"mode":"burn"}"#,
        std::time::Duration::from_secs(30),
    )
    .expect("hostd stopped the guest within 30 s");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("fuel"), "{stderr}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A guest still running when the wall-clock budget runs out is interrupted (fuel unlimited).
#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn a_guest_past_its_wall_clock_budget_is_interrupted() {
    let (root, _ws, config) = probe_setup(
        "wallclock",
        "[budgets]\nguest_fuel = 0\nwallclock_ms = 500\n",
    );
    let started = std::time::Instant::now();
    let out = support::run_step_within(
        &support::probe_component(),
        &config,
        r#"{"mode":"burn"}"#,
        std::time::Duration::from_secs(30),
    )
    .expect("hostd stopped the guest within 30 s");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("wall-clock"), "{stderr}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn a_read_only_workspace_refuses_every_write() {
    let (root, ws, config) = probe_setup("readonly", "[workspace]\nread_only = true\n");
    // The probe writes its results itself, so a read-only run reports through stderr instead.
    let out = support::run_step_with(
        &support::probe_component(),
        &config,
        r#"{"mode":"write_ops"}"#,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "results.json could not be written either"
    );
    assert!(stderr.contains("read-only"), "{stderr}");
    assert!(
        ws.join("seed.txt").exists() && ws.join("other.txt").exists(),
        "nothing moved or removed"
    );
    assert!(
        !ws.join("new-dir").exists() && !ws.join("small.txt").exists(),
        "nothing created"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn writes_past_max_file_bytes_are_refused() {
    let (root, ws, config) = probe_setup("maxfile", "[workspace]\nmax_file_bytes = 1024\n");
    let results = probe_results(&config, &ws, serde_json::json!({ "mode": "write_ops" }));
    assert_eq!(results["small_write"], "allowed", "{results}");
    assert!(
        results["big_write"]
            .as_str()
            .unwrap_or_default()
            .starts_with("refused"),
        "{results}"
    );
    assert_eq!(results["read"], "seed", "reads still work");
    let _ = std::fs::remove_dir_all(&root);
}

/// The policy capability reports the configured budgets and rules, and enforces the budgets.
#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn the_policy_capability_reports_and_enforces_the_budgets() {
    let (root, ws, config) = probe_setup(
        "policy",
        "allow_proc = [\"echo\"]\n[budgets]\nprocesses = 4\nfilesystem_ops = 100\n",
    );
    let results = probe_results(&config, &ws, serde_json::json!({ "mode": "policy" }));
    assert_eq!(results["budgets"]["processes"], 4, "{results}");
    assert_eq!(results["budgets"]["filesystemops"], 100, "{results}");
    assert_eq!(
        results["commands"],
        serde_json::json!(["echo"]),
        "{results}"
    );
    assert_eq!(results["read_only"], false);
    assert!(
        results["claim_small"]
            .as_u64()
            .is_some_and(|used| used >= 3),
        "{results}"
    );
    assert!(
        results["claim_huge"]
            .as_str()
            .unwrap_or_default()
            .starts_with("refused"),
        "{results}"
    );
    assert_eq!(
        results["grant"], false,
        "no grant is configured, so none is approved"
    );
    assert_eq!(results["log_event"], "allowed");
    let _ = std::fs::remove_dir_all(&root);
}

/// The WIT browser interface works, under the same host allowlist as the planner's actions.
#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn the_wit_browser_follows_the_host_allowlist() {
    let (wd_port, wd_seen) = support::fake_webdriver();
    let extra = format!(
        "[browser]\nwebdriver_url = \"http://127.0.0.1:{wd_port}\"\nallowed_hosts = [\"login.example.test\"]\n"
    );
    let (root, ws, config) = probe_setup("witbrowser", &extra);
    let results = probe_results(
        &config,
        &ws,
        serde_json::json!({
            "mode": "browser",
            "allowed": "http://login.example.test/",
            "denied": "http://evil.example.test/",
        }),
    );
    assert_eq!(results["open"], "allowed", "{results}");
    assert_eq!(
        results["goto_allowed"], "http://login.example.test/",
        "{results}"
    );
    assert_eq!(results["title"], "Fake title");
    assert!(
        results["goto_off_list"]
            .as_str()
            .unwrap_or_default()
            .starts_with("refused")
    );
    assert!(
        results["goto_file"]
            .as_str()
            .unwrap_or_default()
            .starts_with("refused")
    );
    assert_eq!(results["element_text"], "fake text", "{results}");
    let wd = wd_seen.lock().expect("webdriver log");
    assert!(
        !wd.iter()
            .any(|(_, _, b)| b.contains("evil.example.test") || b.contains("file://")),
        "refused URLs never reached the WebDriver: {wd:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// wasi-warden ships no input backend: input calls are refused as disabled.
#[test]
#[ignore = "needs probe_agent.wasm: cargo build -p probe-agent --target wasm32-wasip2 --release"]
fn input_is_refused_as_disabled() {
    let (root, ws, config) = probe_setup("input", "");
    let results = probe_results(&config, &ws, serde_json::json!({ "mode": "input" }));
    let verdict = results["key_sequence"].as_str().unwrap_or_default();
    assert!(
        verdict.starts_with("refused") && verdict.contains("disabled"),
        "{verdict}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
