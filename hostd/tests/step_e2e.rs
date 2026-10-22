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
    assert!(
        out.stdout.is_empty() && stderr.contains("planner completed task"),
        "the log lines go to stderr: {}",
        String::from_utf8_lossy(&out.stdout)
    );

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

/// A plan that keeps asking for `actions` and never completes.
fn never_completes(actions: serde_json::Value) -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "status": "continue",
        "thought": "again",
        "actions": actions
    })]
}

/// A workspace and a config with `extra` sections for an LLM on `port`; returns (root, config).
fn setup(name: &str, port: u16, extra: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = support::scratch_dir(name);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, port, None);
    support::append_config(&config, extra);
    (root, config)
}

/// The observation data the planner was sent in request `n` (0-based).
fn observed(seen: &[String], n: usize) -> serde_json::Value {
    let request: serde_json::Value = serde_json::from_str(&seen[n]).expect("request JSON");
    let content = request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let data = content
        .split_once("Observation data: ")
        .map(|(_, d)| d)
        .unwrap_or("{}");
    serde_json::from_str(data).unwrap_or_default()
}

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_step_budget_stops_a_plan_that_never_completes() {
    let (port, seen) = support::fake_llm(never_completes(serde_json::json!([
        { "capability": "fs.list_dir", "input": {} }
    ])));
    let (root, config) = setup("steps", port, "[budgets]\nsteps = 2\n");
    let out = support::run_step(&config, "Never finish");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("step budget"), "{stderr}");
    assert_eq!(
        seen.lock().expect("log").len(),
        2,
        "two planner rounds, as budgeted"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_token_budget_stops_the_loop() {
    let (port, seen) = support::fake_llm_costing(
        never_completes(serde_json::json!([{ "capability": "fs.list_dir", "input": {} }])),
        600,
    );
    let (root, config) = setup("tokens", port, "[budgets]\nllm_tokens = 1000\n");
    let out = support::run_step(&config, "Spend tokens");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("token budget"), "{stderr}");
    assert_eq!(
        seen.lock().expect("log").len(),
        2,
        "the second request spent past the budget"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_process_and_filesystem_budgets_refuse_extra_operations() {
    let (port, seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "use up the budgets",
            "actions": [
                { "capability": "proc.spawn", "input": { "command": "echo", "args": ["one"] } },
                { "capability": "proc.spawn", "input": { "command": "echo", "args": ["two"] } },
                { "capability": "fs.list_dir", "input": {} },
                { "capability": "fs.list_dir", "input": {} }
            ]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (root, config) = setup(
        "procfs",
        port,
        "[budgets]\nprocesses = 1\nfilesystem_ops = 1\n",
    );
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("allow_proc = []", "allow_proc = [\"echo\"]");
    std::fs::write(&config, text).unwrap();
    let out = support::run_step(&config, "Spend budgets");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let seen = seen.lock().expect("log");
    let actions = observed(&seen, 1)["actions"].clone();
    let ok: Vec<bool> = actions
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a["success"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        ok,
        [true, false, true, false],
        "one of each, as budgeted: {actions}"
    );
    assert!(
        actions[1]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("budget"),
        "{actions}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An endpoint that reports no usage is charged an estimate, so the token budget still holds.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_token_budget_holds_without_a_usage_report() {
    let (port, seen) = support::fake_llm_reporting(
        never_completes(serde_json::json!([{ "capability": "fs.list_dir", "input": {} }])),
        None,
    );
    let (root, config) = setup("nousage", port, "[budgets]\nllm_tokens = 1000\n");
    let out = support::run_step(&config, "Spend tokens");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("token budget"), "{stderr}");
    assert!(
        seen.lock().expect("log").len() < 8,
        "the token budget, not the step budget, ended the run"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The planner's processes get no more time than the run has left, and once the wall-clock budget
/// is spent the plan's remaining actions are refused.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn actions_stop_at_the_wall_clock_budget() {
    let (port, _seen) = support::fake_llm(never_completes(serde_json::json!([
        { "capability": "proc.spawn", "input": { "command": "sleep", "args": ["30"] } },
        { "capability": "proc.spawn", "input": { "command": "echo", "args": ["late"] } }
    ])));
    // Well past compiling the component, even under load, so the plan runs before the budget ends.
    let (root, config) = setup("wallclock", port, "[budgets]\nwallclock_ms = 10000\n");
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("allow_proc = []", "allow_proc = [\"sleep\", \"echo\"]");
    std::fs::write(&config, text).unwrap();
    let out = support::run_step(&config, "Wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("planner requested capability executions"),
        "the plan ran: {stderr}"
    );
    assert!(
        !stderr.contains("inside a host call"),
        "the process stopped at the budget, before the watchdog: {stderr}"
    );
    assert!(
        stderr.contains("action failed") && stderr.contains("wallclock"),
        "the next action was refused: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A plan runs at most 32 actions, and the host's log lines hold no more than a bounded slice of
/// what the planner wrote (its thought here is 100 KB).
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_plan_is_capped_in_actions_and_in_the_log() {
    // First, an action with a 100 KB capability name: its failure is logged, cut.
    let actions: Vec<serde_json::Value> =
        std::iter::once(serde_json::json!({ "capability": "y".repeat(100_000), "input": {} }))
            .chain((0..99).map(|_| serde_json::json!({ "capability": "fs.list_dir", "input": {} })))
            .collect();
    let (port, seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "x".repeat(100_000),
            "actions": actions
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (root, config) = setup("plancap", port, "");
    let out = support::run_step(&config, "Plan a lot");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        !stderr.contains(&"x".repeat(2_000)),
        "the thought is cut in the log ({} bytes of stderr)",
        stderr.len()
    );
    assert!(
        stderr.contains("action failed") && !stderr.contains(&"y".repeat(2_000)),
        "a failed action's capability and error are cut in the log ({} bytes of stderr)",
        stderr.len()
    );
    let seen = seen.lock().expect("log");
    let request: serde_json::Value = serde_json::from_str(&seen[1]).expect("request JSON");
    let content = request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let observed: serde_json::Value = serde_json::from_str(
        content
            .split_once("Observation data: ")
            .map(|(_, d)| d)
            .unwrap_or("{}"),
    )
    .expect("observation JSON");
    let reports = observed["actions"].as_array().expect("actions");
    assert_eq!(
        reports.len(),
        33,
        "32 actions ran, and one report says the rest did not"
    );
    assert!(
        reports[32]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("at most 32"),
        "{}",
        reports[32]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The capabilities the host runs: every one is shown to the planner.
const CAPABILITIES: [&str; 11] = [
    "fs.list_dir",
    "fs.read_file",
    "proc.spawn",
    "browser.open_session",
    "browser.session.goto",
    "browser.session.describe_page",
    "browser.session.find",
    "browser.element.click",
    "browser.element.type_text",
    "browser.element.inner_text",
    "browser.session.screenshot",
];

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_planner_is_shown_every_capability_the_host_runs() {
    let (port, seen) = support::fake_llm(vec![serde_json::json!({
        "status": "complete", "thought": "done", "actions": [], "reason": "done"
    })]);
    let (root, config) = setup("catalog", port, "");
    let out = support::run_step(&config, "Look around");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = seen.lock().expect("log");
    let request: serde_json::Value = serde_json::from_str(&seen[0]).expect("request JSON");
    let system = request["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    for name in CAPABILITIES {
        assert!(
            system.contains(name),
            "{name} is missing from the prompt:\n{system}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A reply that is not a plan is answered with the parse error, once, instead of ending the run.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_reply_that_is_not_a_plan_is_asked_again() {
    let (port, seen) = support::fake_llm(vec![
        serde_json::json!("I will list the files now."),
        serde_json::json!({
            "status": "continue",
            "thought": "list",
            "actions": [{ "capability": "fs.list_dir", "input": {} }]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (root, config) = setup("replan", port, "");
    let out = support::run_step(&config, "List files");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = seen.lock().expect("log");
    assert_eq!(seen.len(), 3, "the bad reply, the retry, then completion");
    assert!(
        seen[1].contains("could not be parsed"),
        "the retry names the problem: {}",
        seen[1]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Every capability call becomes one JSON line in the audit log: the planner's actions, and the
/// WIT calls the agent makes (here, to the LLM).
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn every_capability_call_is_audited() {
    let (port, _seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "two actions",
            "actions": [
                { "capability": "fs.list_dir", "input": {} },
                { "capability": "proc.spawn", "input": { "command": "echo", "args": ["hi"] } }
            ]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let root = support::scratch_dir("audit");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, port, None);
    let audit = root.join("audit.jsonl");
    support::append_config(
        &config,
        &format!("[audit]\npath = {:?}\n", audit.display().to_string()),
    );
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("allow_proc = []", "allow_proc = [\"echo\"]");
    std::fs::write(&config, text).unwrap();
    let out = support::run_step(&config, "Audit me");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&audit)
        .expect("the audit log")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();
    let actions: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["source"] == "action").collect();
    let names: Vec<&str> = actions
        .iter()
        .map(|e| e["capability"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fs.list_dir", "proc.spawn"], "{events:?}");
    for e in &actions {
        assert_eq!(e["ok"], true, "{e}");
        assert_eq!(e["output_sha256"].as_str().map(str::len), Some(64), "{e}");
        assert!(
            e["ms"].is_u64() && e["run"].is_string() && e["step"].is_u64(),
            "{e}"
        );
    }
    let llm_calls = events
        .iter()
        .filter(|e| e["source"] == "wit" && e["capability"] == "llm.complete")
        .count();
    assert_eq!(llm_calls, 2, "both rounds' LLM calls: {events:?}");
    let runs: std::collections::BTreeSet<&str> = events
        .iter()
        .map(|e| e["run"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(runs.len(), 1, "one run id");
    let _ = std::fs::remove_dir_all(&root);
}

/// A `[[llm.providers]]` entry for a provider on `port`.
fn provider(name: &str, tier: &str, port: u16, input_price: f64, output_price: f64) -> String {
    format!(
        "[[llm.providers]]\nname = {name:?}\ntier = {tier:?}\napi_base = \"http://127.0.0.1:{port}/v1\"\napi_key = \"test\"\nmodel = \"{name}-model\"\ninput_price = {input_price:?}\noutput_price = {output_price:?}\n"
    )
}

/// A workspace and a config with `providers` (in place of the single `[llm]`) and `extra`.
fn router_setup(
    name: &str,
    providers: &str,
    extra: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = support::scratch_dir(name);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let config = root.join("hostd.toml");
    std::fs::write(
        &config,
        format!(
            "workspace_root = {:?}\n{extra}\n{providers}",
            workspace.display().to_string()
        ),
    )
    .expect("config");
    (root, config)
}

fn list_then_complete() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "status": "continue",
            "thought": "list",
            "actions": [{ "capability": "fs.list_dir", "input": {} }]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]
}

/// The router tries the local provider first, though the config lists the frontier one first; when
/// the local one fails, the frontier one answers. Each attempt is a version-1 TelemetryEvent with
/// its provider, outcome, tokens, cost and latency.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_failing_local_endpoint_falls_back_to_the_frontier() {
    let (local, local_seen) = support::failing_llm(503);
    let (frontier, frontier_seen) = support::fake_llm_costing(list_then_complete(), 1000);
    let root = support::scratch_dir("telemetry-root");
    let telemetry = root.join("telemetry.jsonl");
    let (run_root, config) = router_setup(
        "fallback",
        &format!(
            "{}{}",
            provider("frontier", "frontier", frontier, 2.0, 10.0),
            provider("local", "local", local, 0.0, 0.0)
        ),
        &format!(
            "[telemetry]\npath = {:?}\n",
            telemetry.display().to_string()
        ),
    );
    let out = support::run_step(&config, "List workspace files");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        *local_seen.lock().unwrap(),
        2,
        "local was tried first each round"
    );
    assert_eq!(
        frontier_seen.lock().unwrap().len(),
        2,
        "and the frontier answered"
    );
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&telemetry)
        .expect("telemetry")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is JSON"))
        .collect();
    assert_eq!(events.len(), 4, "{events:?}");
    for pair in events.chunks(2) {
        let (tried, answered) = (&pair[0], &pair[1]);
        assert_eq!(tried["version"], 1);
        assert_eq!(
            (tried["provider"].as_str(), tried["ok"].as_bool()),
            (Some("local"), Some(false))
        );
        assert!(
            tried["error"].as_str().unwrap_or_default().contains("503"),
            "{tried}"
        );
        assert_eq!(answered["provider"], "frontier");
        assert_eq!(answered["ok"], true);
        assert_eq!(answered["attempt"], 2);
        assert!(
            answered["route_reason"]
                .as_str()
                .unwrap_or_default()
                .contains("local"),
            "the fallback names what failed: {answered}"
        );
        // The fake reports 500 prompt and 500 completion tokens: 500 * 2.0 + 500 * 10.0 micro-units.
        assert_eq!(answered["cost_micros"], 6000, "{answered}");
        assert!(answered["latency_ms"].is_u64(), "{answered}");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&run_root);
}

/// A local provider that answers is the only one asked.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_working_local_endpoint_answers_alone() {
    let (local, local_seen) = support::fake_llm(list_then_complete());
    let (frontier, frontier_seen) = support::fake_llm(list_then_complete());
    let (root, config) = router_setup(
        "localfirst",
        &format!(
            "{}{}",
            provider("frontier", "frontier", frontier, 2.0, 10.0),
            provider("local", "local", local, 0.0, 0.0)
        ),
        "",
    );
    let out = support::run_step(&config, "List workspace files");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(local_seen.lock().unwrap().len(), 2);
    assert!(
        frontier_seen.lock().unwrap().is_empty(),
        "the frontier was never asked"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Once the cost budget is spent, paid providers are no longer tried: with the free one failing,
/// the next LLM call is refused and the run stops.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn paid_providers_stop_at_the_cost_budget() {
    let (local, _local_seen) = support::failing_llm(500);
    let (frontier, frontier_seen) = support::fake_llm_costing(list_then_complete(), 1000);
    let (root, config) = router_setup(
        "costcap",
        &format!(
            "{}{}",
            provider("local", "local", local, 0.0, 0.0),
            provider("frontier", "frontier", frontier, 2.0, 10.0)
        ),
        "[budgets]\nllm_cost_micros = 5000\n",
    );
    let out = support::run_step(&config, "List workspace files");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("llm_cost_micros"),
        "the cost budget is named: {stderr}"
    );
    assert_eq!(
        frontier_seen.lock().unwrap().len(),
        1,
        "the first answer cost 6000 of 5000; no paid call after it"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A reply without content counts as the provider failing: the next one is asked.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_reply_without_content_falls_back() {
    let (local, local_seen) = support::empty_llm();
    let (frontier, frontier_seen) = support::fake_llm(list_then_complete());
    let (root, config) = router_setup(
        "emptyreply",
        &format!(
            "{}{}",
            provider("local", "local", local, 0.0, 0.0),
            provider("frontier", "frontier", frontier, 0.0, 0.0)
        ),
        "",
    );
    let out = support::run_step(&config, "List workspace files");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        local_seen.lock().unwrap().len(),
        2,
        "local was asked each round"
    );
    assert_eq!(
        frontier_seen.lock().unwrap().len(),
        2,
        "and the frontier answered each time"
    );
    let _ = std::fs::remove_dir_all(&root);
}
