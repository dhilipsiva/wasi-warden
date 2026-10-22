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
    let created = wd
        .iter()
        .find(|(m, p, _)| m == "POST" && p == "/session")
        .map(|(_, _, b)| b.clone())
        .unwrap_or_default();
    assert!(
        !created.contains("--no-sandbox"),
        "Chrome keeps its sandbox by default: {created}"
    );
    let llm = llm_seen.lock().expect("llm log");
    assert_eq!(llm.len(), 2, "two planner rounds");
    let reported = llm[1].contains(target) && llm[1].contains("browser.session.goto");
    assert!(reported, "the second round saw the navigation: {}", llm[1]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Navigation is confined to the configured hosts: a host off the list and a non-web scheme never
/// reach the WebDriver, and a page that redirects off the list is left for `about:blank`.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn navigation_stays_inside_the_host_allowlist() {
    let root = support::scratch_dir("allowlist");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let goto = |url: &str| {
        serde_json::json!({
            "capability": "browser.session.goto",
            "input": { "session": "s1", "url": url, "timeout_ms": 0 }
        })
    };
    let (llm_port, llm_seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "try five pages",
            "actions": [
                {
                    "capability": "browser.open_session",
                    "input": { "alias": "s1", "allow_downloads": true }
                },
                goto("http://evil.example.test/"),
                goto("file:///etc/passwd"),
                goto("HTTP://LOGIN.example.test"),
                goto("http://redirect.example.test/"),
                goto("http://late.example.test/"),
                {
                    "capability": "browser.session.describe_page",
                    "input": { "session": "s1" }
                },
            ]
        }),
        serde_json::json!({
            "status": "complete",
            "thought": "done",
            "actions": [],
            "result": {},
            "reason": "done"
        }),
    ]);
    let (wd_port, wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));

    let out = support::run_step(&config, "Visit the pages");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "hostd failed:\n{stderr}");

    let wd = wd_seen.lock().expect("webdriver log");
    let sent: Vec<&str> = wd
        .iter()
        .filter(|(m, p, _)| m == "POST" && p.ends_with("/url"))
        .map(|(_, _, b)| b.as_str())
        .collect();
    assert!(
        !sent
            .iter()
            .any(|b| b.contains("evil.example.test") || b.contains("file://")),
        "refused URLs never reached the WebDriver: {sent:?}"
    );
    assert!(
        sent.iter()
            .any(|b| b.contains("\"http://login.example.test/\"")),
        "an allowed host was visited, as the parsed URL: {sent:?}"
    );
    let blanks = sent.iter().filter(|b| b.contains("about:blank")).count();
    assert_eq!(
        blanks, 2,
        "the redirect, and the later wandering off the list, were left for about:blank: {sent:?}"
    );
    let created = wd
        .iter()
        .find(|(m, p, _)| m == "POST" && p == "/session")
        .map(|(_, _, b)| b.clone())
        .unwrap_or_default();
    let workspace_dir = std::fs::canonicalize(&workspace).expect("workspace");
    let download_pref = format!(
        "\"download.default_directory\":{:?}",
        workspace_dir.display().to_string()
    );
    assert!(
        created.contains(&download_pref),
        "downloads land in the workspace: {created}"
    );
    let llm = llm_seen.lock().expect("llm log");
    let report: serde_json::Value = serde_json::from_str(&llm[1]).expect("request JSON");
    let content = report["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let data = content
        .split_once("Observation data: ")
        .map(|(_, d)| d)
        .unwrap_or_default();
    let observed: serde_json::Value = serde_json::from_str(data).expect("observation JSON");
    let ok: Vec<bool> = observed["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a["success"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        ok,
        [true, false, false, true, false, true, false],
        "which actions succeeded (the last, on a page that wandered off the list)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The page check treats chromedriver's starting page (`data:,`) as blank, and checks the page
/// behind an element before acting on it: an element on a page that wandered off is refused.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_fresh_session_works_and_element_actions_recheck_the_page() {
    let root = support::scratch_dir("elements");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (llm_port, llm_seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "describe first, then an element on a page that wanders off",
            "actions": [
                { "capability": "browser.open_session", "input": { "alias": "s1" } },
                { "capability": "browser.session.describe_page", "input": { "session": "s1" } },
                {
                    "capability": "browser.session.goto",
                    "input": { "session": "s1", "url": "http://late.example.test/", "timeout_ms": 0 }
                },
                {
                    "capability": "browser.session.find",
                    "input": {
                        "session": "s1",
                        "alias": "e1",
                        "selector": { "kind": "css", "value": "#greeting" },
                        "timeout_ms": 0
                    }
                }
            ]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (wd_port, _wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));
    let out = support::run_step(&config, "Browse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let llm = llm_seen.lock().expect("llm log");
    let report: serde_json::Value = serde_json::from_str(&llm[1]).expect("request JSON");
    let content = report["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let data = content
        .split_once("Observation data: ")
        .map(|(_, d)| d)
        .unwrap_or_default();
    let observed: serde_json::Value = serde_json::from_str(data).expect("observation JSON");
    let ok: Vec<bool> = observed["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a["success"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        ok,
        [true, true, true, false],
        "the fresh page is fine; the wandered one is not: {observed}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_browser_action_budget_refuses_extra_actions() {
    let root = support::scratch_dir("browserbudget");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (llm_port, llm_seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "three browser actions",
            "actions": [
                { "capability": "browser.open_session", "input": { "alias": "s1" } },
                {
                    "capability": "browser.session.goto",
                    "input": { "session": "s1", "url": "http://login.example.test/", "timeout_ms": 0 }
                },
                { "capability": "browser.session.describe_page", "input": { "session": "s1" } }
            ]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (wd_port, _wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));
    support::append_config(&config, "[budgets]\nbrowser_actions = 2\n");
    let out = support::run_step(&config, "Browse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let llm = llm_seen.lock().expect("llm log");
    let report: serde_json::Value = serde_json::from_str(&llm[1]).expect("request JSON");
    let content = report["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    let data = content
        .split_once("Observation data: ")
        .map(|(_, d)| d)
        .unwrap_or_default();
    let observed: serde_json::Value = serde_json::from_str(data).expect("observation JSON");
    let ok: Vec<bool> = observed["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a["success"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(ok, [true, true, false], "two browser actions, as budgeted");
    let _ = std::fs::remove_dir_all(&root);
}

/// The planted secret value of these tests.
const SECRET: &str = "planted-s3cret-9f2c";

/// Whether `text` holds the secret.
fn leaks(text: &str) -> bool {
    text.contains(SECRET)
}

/// A secret named in the plan reaches the page it is typed into and the process it is given to,
/// but never the model, the audit log or the host's own log lines.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn a_secret_reaches_the_page_but_never_the_model_or_the_logs() {
    let root = support::scratch_dir("secret");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (llm_port, llm_seen) = support::fake_llm(vec![
        serde_json::json!({
            "status": "continue",
            "thought": "type the password, and hand it to a process",
            "actions": [
                { "capability": "browser.open_session", "input": { "alias": "s1" } },
                {
                    "capability": "browser.session.goto",
                    "input": { "session": "s1", "url": "http://login.example.test/", "timeout_ms": 0 }
                },
                {
                    "capability": "browser.session.find",
                    "input": {
                        "session": "s1",
                        "alias": "pw",
                        "selector": { "kind": "css", "value": "input#password" },
                        "timeout_ms": 0
                    }
                },
                {
                    "capability": "browser.element.type_text",
                    "input": { "element": "pw", "text": "{{secret:demo_password}}", "submit": false }
                },
                {
                    "capability": "proc.spawn",
                    "input": {
                        "command": "env",
                        "env": [{ "key": "TOKEN", "value": "{{secret:demo_password}}" }]
                    }
                }
            ]
        }),
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "done" }),
    ]);
    let (wd_port, wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));
    let audit = root.join("audit.jsonl");
    support::append_config(
        &config,
        &format!(
            "allow_env = [\"TOKEN\"]\n[secrets]\ndemo_password = {SECRET:?}\n[audit]\npath = {:?}\n",
            audit.display().to_string()
        ),
    );
    // `allow_env` is a top-level key: it must come before the tables.
    let text = std::fs::read_to_string(&config).unwrap();
    let (before, after) = text.split_once("allow_env").unwrap();
    let (allow_env_line, rest) = after.split_once('\n').unwrap();
    let text = format!("allow_env{allow_env_line}\n{before}{rest}")
        .replace("allow_proc = []", "allow_proc = [\"env\"]");
    std::fs::write(&config, text).unwrap();

    let out = support::run_step(&config, "Log in");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let wd = wd_seen.lock().expect("webdriver log");
    assert!(
        wd.iter()
            .any(|(m, p, b)| m == "POST" && p.ends_with("/value") && leaks(b)),
        "the secret was typed into the page: {wd:?}"
    );
    let llm = llm_seen.lock().expect("llm log");
    assert!(
        !llm.iter().any(|r| leaks(r)),
        "the model never saw it: {llm:?}"
    );
    assert!(
        llm[1].contains("TOKEN={{secret:demo_password}}"),
        "the process saw it, and its output came back redacted: {}",
        llm[1]
    );
    assert!(!leaks(&stderr), "nor the host's log lines");
    let audit = std::fs::read_to_string(&audit).expect("the audit log");
    assert!(!audit.is_empty() && !leaks(&audit), "nor the audit log");
    let _ = std::fs::remove_dir_all(&root);
}

/// docs/browser-login-task.md's plan, run as written against a fake site: the credentials go from
/// the host's secrets into the form, and never to the model.
#[test]
#[ignore = "needs agent_core.wasm: cargo build -p agent-core --target wasm32-wasip2 --release"]
fn the_login_walkthrough_runs() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/browser-login-task.md"
    ))
    .expect("the walkthrough");
    let plan_json = doc
        .split("```json")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("a JSON plan in the walkthrough");
    let plan: serde_json::Value = serde_json::from_str(plan_json).expect("the plan is JSON");
    let root = support::scratch_dir("walkthrough");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (llm_port, llm_seen) = support::fake_llm(vec![
        plan,
        serde_json::json!({ "status": "complete", "thought": "done", "actions": [], "reason": "logged in" }),
    ]);
    let (wd_port, wd_seen) = support::fake_webdriver();
    let config = root.join("hostd.toml");
    support::write_config(&config, &workspace, llm_port, Some(wd_port));
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("allowed_hosts = [", "allowed_hosts = [\"demo.example\", ");
    std::fs::write(&config, text).unwrap();
    support::append_config(
        &config,
        &format!("[secrets]\ndemo_username = \"demo-user\"\ndemo_password = {SECRET:?}\n"),
    );
    let out = support::run_step(&config, "Log into https://demo.example/login");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let llm = llm_seen.lock().expect("llm log");
    let observed: serde_json::Value = {
        let report: serde_json::Value = serde_json::from_str(&llm[1]).expect("request JSON");
        let content = report["messages"][1]["content"]
            .as_str()
            .unwrap_or_default();
        serde_json::from_str(
            content
                .split_once("Observation data: ")
                .map(|(_, d)| d)
                .unwrap_or("{}"),
        )
        .expect("observation JSON")
    };
    let failed: Vec<&serde_json::Value> = observed["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .filter(|a| a["success"] != true)
        .collect();
    assert!(
        failed.is_empty(),
        "every step of the walkthrough ran: {failed:?}"
    );
    let wd = wd_seen.lock().expect("webdriver log");
    let typed: Vec<&str> = wd
        .iter()
        .filter(|(m, p, _)| m == "POST" && p.ends_with("/value"))
        .map(|(_, _, b)| b.as_str())
        .collect();
    assert!(typed.iter().any(|b| b.contains("demo-user")), "{typed:?}");
    assert!(typed.iter().any(|b| leaks(b)), "{typed:?}");
    assert!(
        !llm.iter().any(|r| leaks(r) || r.contains("demo-user")),
        "the model never saw them"
    );
    let _ = std::fs::remove_dir_all(&root);
}
