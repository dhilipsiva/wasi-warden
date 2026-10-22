//! Shared pieces of the end-to-end tests: the agent component, scratch directories, a scripted
//! OpenAI-compatible endpoint, a fake WebDriver, and running `hostd step`.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

/// The agent component: `AGENT_CORE_WASM`, or the release build in this test's target directory.
pub fn component() -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_CORE_WASM") {
        return PathBuf::from(path);
    }
    release_wasm("agent_core", "agent-core")
}

/// The probe test guest (tests/probe-agent): `PROBE_AGENT_WASM`, or its release build.
pub fn probe_component() -> PathBuf {
    if let Some(path) = std::env::var_os("PROBE_AGENT_WASM") {
        return PathBuf::from(path);
    }
    release_wasm("probe_agent", "probe-agent")
}

/// `<target>/wasm32-wasip2/release/<file>.wasm` for this test's target directory.
fn release_wasm(file: &str, package: &str) -> PathBuf {
    // <target>/debug/deps/<test>-<hash> -> <target>
    let exe = std::env::current_exe().expect("current exe");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("the target directory above debug/deps");
    let path = target.join(format!("wasm32-wasip2/release/{file}.wasm"));
    assert!(
        path.exists(),
        "{} is missing: cargo build -p {package} --target wasm32-wasip2 --release",
        path.display()
    );
    path
}

/// A fresh directory under the system temp directory.
pub fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("hostd-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One HTTP/1.1 request read off `reader`: method, path and body; `None` when the peer closed.
fn read_request(reader: &mut BufReader<TcpStream>) -> Option<(String, String, String)> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some((method, path, String::from_utf8_lossy(&body).into_owned()))
}

fn respond(stream: &mut TcpStream, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

/// Serve `handle` on a loopback port: each connection on its own thread, keep-alive honoured.
fn serve(handle: impl Fn(&str, &str, &str) -> String + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = Arc::new(handle);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let handle = Arc::clone(&handle);
            thread::spawn(move || {
                let mut writer = stream.try_clone().expect("clone stream");
                let mut reader = BufReader::new(stream);
                while let Some((method, path, body)) = read_request(&mut reader) {
                    let reply = handle(&method, &path, &body);
                    respond(&mut writer, &reply);
                }
            });
        }
    });
    port
}

/// The fake servers are on loopback; a proxy from the environment would intercept them.
const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// A chat-completions endpoint answering each request with the next of `plans` (the last repeats);
/// returns its port and the request bodies it saw.
pub fn fake_llm(plans: Vec<serde_json::Value>) -> (u16, Arc<Mutex<Vec<String>>>) {
    fake_llm_costing(plans, 2)
}

/// Like [`fake_llm`], reporting `tokens` total tokens used per request.
pub fn fake_llm_costing(
    plans: Vec<serde_json::Value>,
    tokens: u32,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    fake_llm_reporting(
        plans,
        Some(serde_json::json!({
            "prompt_tokens": tokens / 2,
            "completion_tokens": tokens - tokens / 2,
            "total_tokens": tokens
        })),
    )
}

/// A chat-completions endpoint that takes `delay` to answer each request (with a plan to finish).
pub fn slow_llm(delay: std::time::Duration) -> u16 {
    serve(move |_method, _path, _body| {
        thread::sleep(delay);
        serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "{\"status\":\"complete\",\"thought\":\"late\",\"actions\":[]}" },
                "finish_reason": "stop"
            }]
        })
        .to_string()
    })
}

/// Like [`fake_llm`], with `usage` as each response's usage report (`None`: no report).
pub fn fake_llm_reporting(
    plans: Vec<serde_json::Value>,
    usage: Option<serde_json::Value>,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let port = serve(move |_method, _path, body| {
        let n = {
            let mut seen = log.lock().expect("log");
            seen.push(body.to_string());
            seen.len()
        };
        let plan = &plans[(n - 1).min(plans.len() - 1)];
        let mut response = serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": plan.to_string() },
                "finish_reason": "stop"
            }]
        });
        if let Some(usage) = &usage {
            response["usage"] = usage.clone();
        }
        response.to_string()
    });
    (port, seen)
}

/// The requests a fake server saw: method, path and body.
pub type Requests = Arc<Mutex<Vec<(String, String, String)>>>;

/// The one session a [`fake_webdriver`] hands out.
pub const SESSION: &str = "fake-session-1";

/// A W3C WebDriver that opens one session, remembers the URL it is sent to, and answers everything
/// else with a null value; returns its port and the requests (method, path, body) it saw.
pub fn fake_webdriver() -> (u16, Requests) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    // chromedriver starts a session on `data:,`.
    let url = Arc::new(Mutex::new(String::from("data:,")));
    // A page on `late.example.test` navigates away by itself after it was first looked at.
    let late_reads = Arc::new(Mutex::new(0u32));
    let port = serve(move |method, path, body| {
        log.lock()
            .expect("log")
            .push((method.to_string(), path.to_string(), body.to_string()));
        let session_url = format!("/session/{SESSION}/url");
        let value = match (method, path) {
            ("POST", "/session") => serde_json::json!({
                "sessionId": SESSION,
                "capabilities": { "browserName": "chrome" }
            }),
            ("GET", "/status") => serde_json::json!({ "ready": true, "message": "fake" }),
            ("GET", p) if p == format!("/session/{SESSION}/title") => {
                serde_json::json!("Fake title")
            }
            ("POST", p) if p == format!("/session/{SESSION}/element") => {
                serde_json::json!({ "element-6066-11e4-a52e-4f735466cecf": "fake-element-1" })
            }
            ("POST", p) if p == format!("/session/{SESSION}/elements") => {
                // `#missing` is never on the page.
                if body.contains("missing") {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{ "element-6066-11e4-a52e-4f735466cecf": "fake-element-1" }])
                }
            }
            ("GET", p) if p == format!("/session/{SESSION}/element/fake-element-1/text") => {
                serde_json::json!("fake text")
            }
            ("POST", p) if p == session_url => {
                let sent: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                if let Some(u) = sent["url"].as_str() {
                    // A page on `redirect.example.test` sends the browser somewhere else.
                    let landed = if u.contains("redirect.example.test") {
                        "http://elsewhere.invalid/"
                    } else {
                        u
                    };
                    *url.lock().expect("url") = landed.to_string();
                }
                serde_json::Value::Null
            }
            ("GET", p) if p == session_url => {
                let mut current = url.lock().expect("url");
                if current.contains("late.example.test") {
                    let mut reads = late_reads.lock().expect("reads");
                    *reads += 1;
                    if *reads > 1 {
                        *current = "http://elsewhere.invalid/".to_string();
                    }
                }
                serde_json::json!(*current)
            }
            _ => serde_json::Value::Null,
        };
        serde_json::json!({ "value": value }).to_string()
    });
    (port, seen)
}

/// A `hostd.toml` for `workspace`, the LLM on `llm_port`, and a WebDriver on `webdriver_port` that
/// may visit `login.example.test`, `redirect.example.test` and `late.example.test`.
pub fn write_config(path: &Path, workspace: &Path, llm_port: u16, webdriver_port: Option<u16>) {
    let mut config = format!(
        "workspace_root = {:?}\nallow_proc = []\n\n[llm]\napi_base = \"http://127.0.0.1:{llm_port}/v1\"\napi_key = \"test\"\nmodel = \"scripted\"\n",
        workspace.display().to_string()
    );
    if let Some(port) = webdriver_port {
        config.push_str(&format!(
            "\n[browser]\nwebdriver_url = \"http://127.0.0.1:{port}\"\nallowed_hosts = [\"login.example.test\", \"redirect.example.test\", \"late.example.test\"]\n"
        ));
    }
    std::fs::write(path, config).expect("config");
}

/// Append `extra` (whole TOML sections, such as `[budgets]`) to the config at `path`.
pub fn append_config(path: &Path, extra: &str) {
    let mut config = std::fs::read_to_string(path).expect("config");
    config.push('\n');
    config.push_str(extra);
    std::fs::write(path, config).expect("config");
}

/// Run `hostd step` like [`run_step_with`], killing it after `limit`; `None` if it had to be killed.
pub fn run_step_within(
    component: &Path,
    config: &Path,
    task: &str,
    limit: std::time::Duration,
) -> Option<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hostd"));
    cmd.arg("step")
        .arg("--component")
        .arg(component)
        .arg("--config")
        .arg(config)
        .args(["--task", task, "--observation", "{}"])
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for var in PROXY_VARS {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().expect("run hostd");
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return Some(Output {
                status,
                stdout: stdout.join().expect("stdout"),
                stderr: stderr.join().expect("stderr"),
            });
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // What hostd wrote before it had to be killed, for the failing test's output.
            let stderr = stderr.join().unwrap_or_default();
            eprintln!(
                "hostd was killed after {limit:?}; its stderr:\n{}",
                String::from_utf8_lossy(&stderr)
            );
            return None;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Run `hostd step` for `task` with the agent component and `config`.
pub fn run_step(config: &Path, task: &str) -> Output {
    run_step_with(&component(), config, task)
}

/// Run `hostd step` for `task` with `component` and `config`.
pub fn run_step_with(component: &Path, config: &Path, task: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hostd"));
    cmd.arg("step")
        .arg("--component")
        .arg(component)
        .arg("--config")
        .arg(config)
        .args(["--task", task, "--observation", "{}"])
        .env("RUST_LOG", "info");
    for var in PROXY_VARS {
        cmd.env_remove(var);
    }
    cmd.output().expect("run hostd")
}
