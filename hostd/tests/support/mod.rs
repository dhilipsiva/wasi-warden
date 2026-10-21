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
    // <target>/debug/deps/<test>-<hash> -> <target>
    let exe = std::env::current_exe().expect("current exe");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("the target directory above debug/deps");
    let path = target.join("wasm32-wasip2/release/agent_core.wasm");
    assert!(
        path.exists(),
        "{} is missing: cargo build -p agent-core --target wasm32-wasip2 --release",
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

/// A chat-completions endpoint answering each request with the next of `plans` (the last repeats);
/// returns its port and the request bodies it saw.
pub fn fake_llm(plans: Vec<serde_json::Value>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let port = serve(move |_method, _path, body| {
        let n = {
            let mut seen = log.lock().expect("log");
            seen.push(body.to_string());
            seen.len()
        };
        let plan = &plans[(n - 1).min(plans.len() - 1)];
        serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": plan.to_string() },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
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
    let url = Arc::new(Mutex::new(String::from("about:blank")));
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
            ("POST", p) if p == session_url => {
                let sent: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                if let Some(u) = sent["url"].as_str() {
                    *url.lock().expect("url") = u.to_string();
                }
                serde_json::Value::Null
            }
            ("GET", p) if p == session_url => serde_json::json!(*url.lock().expect("url")),
            _ => serde_json::Value::Null,
        };
        serde_json::json!({ "value": value }).to_string()
    });
    (port, seen)
}

/// A `hostd.toml` for `workspace`, the LLM on `llm_port`, and a WebDriver on `webdriver_port`.
pub fn write_config(path: &Path, workspace: &Path, llm_port: u16, webdriver_port: Option<u16>) {
    let mut config = format!(
        "workspace_root = {:?}\nallow_proc = []\n\n[llm]\napi_base = \"http://127.0.0.1:{llm_port}/v1\"\napi_key = \"test\"\nmodel = \"scripted\"\n",
        workspace.display().to_string()
    );
    if let Some(port) = webdriver_port {
        config.push_str(&format!(
            "\n[browser]\nwebdriver_url = \"http://127.0.0.1:{port}\"\n"
        ));
    }
    std::fs::write(path, config).expect("config");
}

/// Run `hostd step` for `task` with the component and `config`.
pub fn run_step(config: &Path, task: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hostd"))
        .arg("step")
        .arg("--component")
        .arg(component())
        .arg("--config")
        .arg(config)
        .args(["--task", task, "--observation", "{}"])
        .env("RUST_LOG", "info")
        .output()
        .expect("run hostd")
}
