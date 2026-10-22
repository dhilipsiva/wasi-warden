//! A test guest for hostd's end-to-end tests (`hostd/tests/probe_e2e.rs`). Where agent-core only asks
//! the `llm` capability for a plan, this calls the fs and proc capabilities and WASI itself, through
//! the real bindings, and records what each call returned in `results.json` in the workspace. The
//! task is a JSON object: `mode` picks what to probe (`rules`, the default; `write_ops`; `policy`;
//! `browser`; `input`; or `burn`, which loops forever), and the rest names the programs and URLs to
//! try.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "control",
        generate_all,
        default_bindings_module: "bindings",
    });
}

use bindings::exports::osagent::agent::planner::{self, AgentError, CompletePlan, StepResponse};
use bindings::osagent::browser::browser::{self, Selector, SelectorKind, SessionOptions};
use bindings::osagent::common::types::AuditEvent;
use bindings::osagent::fs::fs::{self, DirHandle, FileOpenOptions};
use bindings::osagent::input::input;
use bindings::osagent::policy::policy::{self, BudgetKind, GrantRequest};
use bindings::osagent::proc::proc::{self, EnvVar, SpawnOptions, StdioMode};

struct Probe;

fn read_only() -> FileOpenOptions {
    FileOpenOptions {
        read: true,
        write: false,
        append: false,
        create: false,
        truncate: false,
    }
}

fn spawn_options(argv: &[&str], env: Vec<EnvVar>, timeout_ms: Option<u64>) -> SpawnOptions {
    SpawnOptions {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        working_dir: None,
        env,
        stdin: StdioMode::Null,
        stdout: StdioMode::Pipe,
        stderr: StdioMode::Pipe,
        timeout_ms,
    }
}

/// "refused" for an error, "allowed" otherwise.
fn verdict<T, E>(r: &Result<T, E>) -> Value {
    json!(if r.is_ok() { "allowed" } else { "refused" })
}

fn write_only() -> FileOpenOptions {
    FileOpenOptions {
        read: false,
        write: true,
        append: false,
        create: true,
        truncate: true,
    }
}

/// "refused: <message>" for an error, "allowed" otherwise.
fn outcome<T>(r: &Result<T, bindings::osagent::common::types::CapabilityError>) -> Value {
    match r {
        Ok(_) => json!("allowed"),
        Err(e) => json!(format!("refused: {}", e.message)),
    }
}

/// Every kind of write, for the workspace rules (`read_only`, `max_file_bytes`).
fn probe_writes(ws: &DirHandle) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("ensure_dir".into(), outcome(&fs::ensure_dir(ws, "new-dir")));
    let small =
        fs::open_file(ws, "small.txt", write_only()).and_then(|f| f.write_string("ok", false));
    out.insert("small_write".into(), outcome(&small));
    let big = fs::open_file(ws, "big.txt", write_only()).and_then(|f| f.write(&vec![b'x'; 2048]));
    out.insert("big_write".into(), outcome(&big));
    out.insert(
        "rename".into(),
        outcome(&fs::rename(ws, "seed.txt", "moved.txt")),
    );
    out.insert("remove".into(), outcome(&fs::remove_file(ws, "other.txt")));
    let read = fs::open_file(ws, "seed.txt", read_only()).and_then(|f| f.read_to_string(64));
    out.insert("read".into(), json!(read.unwrap_or_else(|e| e.message)));
    out
}

fn kind_name(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "steps",
        BudgetKind::WallclockMs => "wallclock_ms",
        BudgetKind::LlmTokens => "llm_tokens",
        BudgetKind::Processes => "processes",
        BudgetKind::FilesystemOps => "filesystem_ops",
        BudgetKind::BrowserActions => "browser_actions",
    }
}

/// The policy capability: the snapshot, budget claims, a grant request and an audit event.
fn probe_policy() -> Map<String, Value> {
    let mut out = Map::new();
    match policy::describe() {
        Ok(snapshot) => {
            let budgets: Map<String, Value> = snapshot
                .budgets
                .iter()
                .map(|b| (kind_name(b.kind).to_string(), json!(b.limit)))
                .collect();
            out.insert("budgets".into(), Value::Object(budgets));
            out.insert(
                "read_only".into(),
                json!(snapshot.workspaces.iter().any(|w| w.read_only)),
            );
            out.insert(
                "commands".into(),
                json!(
                    snapshot
                        .commands
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                ),
            );
            out.insert(
                "browser_hosts".into(),
                json!(
                    snapshot
                        .browser
                        .map(|b| b.allowed_hosts)
                        .unwrap_or_default()
                ),
            );
        }
        Err(e) => {
            out.insert("describe".into(), json!(format!("refused: {}", e.message)));
        }
    }
    let claimed = policy::claim_budget(BudgetKind::FilesystemOps, 3);
    out.insert(
        "claim_small".into(),
        match claimed {
            Ok(b) => json!(b.used),
            Err(e) => json!(format!("refused: {}", e.message)),
        },
    );
    out.insert(
        "claim_huge".into(),
        outcome(&policy::claim_budget(BudgetKind::Processes, 1_000_000)),
    );
    let grant = policy::request_capability(&GrantRequest {
        capability: "proc.spawn".into(),
        scope: Some("rm".into()),
        justification: "probe".into(),
    });
    out.insert(
        "grant".into(),
        match grant {
            Ok(g) => json!(g.approved),
            Err(e) => json!(format!("refused: {}", e.message)),
        },
    );
    let logged = policy::log_event(&AuditEvent {
        event_type: "probe".into(),
        step: Some(0),
        payload: "{}".into(),
    });
    out.insert("log_event".into(), outcome(&logged));
    out
}

/// The WIT browser interface against the host's allowlist.
fn probe_browser(task: &Value) -> Map<String, Value> {
    let url = |key: &str| task[key].as_str().unwrap_or_default().to_string();
    let mut out = Map::new();
    let options = SessionOptions {
        profile: None,
        headless: true,
        allow_downloads: false,
    };
    let session = match browser::open_session(&options) {
        Ok(s) => s,
        Err(e) => {
            out.insert("open".into(), json!(format!("refused: {}", e.message)));
            return out;
        }
    };
    out.insert("open".into(), json!("allowed"));
    let visited = session.goto(&url("allowed"), Some(0));
    out.insert(
        "goto_allowed".into(),
        json!(
            visited
                .map(|p| p.url)
                .unwrap_or_else(|e| format!("refused: {}", e.message))
        ),
    );
    let page = session.describe_page(false);
    out.insert(
        "title".into(),
        json!(page.ok().and_then(|p| p.title).unwrap_or_default()),
    );
    out.insert(
        "goto_off_list".into(),
        outcome(&session.goto(&url("denied"), Some(0))),
    );
    out.insert(
        "goto_file".into(),
        outcome(&session.goto("file:///etc/passwd", Some(0))),
    );
    let selector = Selector {
        kind: SelectorKind::Css,
        value: "#greeting".into(),
    };
    let text = session.find(&selector, Some(0)).and_then(|el| {
        el.click()?;
        el.inner_text()
    });
    out.insert(
        "element_text".into(),
        json!(text.unwrap_or_else(|e| format!("refused: {}", e.message))),
    );
    session.close();
    out
}

fn probe(task: &str, ws: &DirHandle) -> Map<String, Value> {
    let task: Value = serde_json::from_str(task).unwrap_or_default();
    match task["mode"].as_str().unwrap_or("rules") {
        "write_ops" => return probe_writes(ws),
        "policy" => return probe_policy(),
        "browser" => return probe_browser(&task),
        "input" => {
            let mut out = Map::new();
            out.insert("key_sequence".into(), outcome(&input::key_sequence("x")));
            return out;
        }
        "burn" => {
            // Loops until the host interrupts it.
            let mut n: u64 = 0;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        _ => {}
    }
    let program = |key: &str| task[key].as_str().unwrap_or_default().to_string();
    let mut out = Map::new();

    // WASI: writes to stdout and stderr, and a sleep on the clocks.
    println!("probe stdout");
    eprintln!("probe stderr");
    std::thread::sleep(Duration::from_millis(10));
    out.insert("wasi".into(), json!("ok"));

    let names: Vec<String> = fs::list_dir(ws)
        .map(|entries| entries.into_iter().map(|e| e.name).collect())
        .unwrap_or_default();
    out.insert("list".into(), json!(names));

    // A directory made and a file written, closed, reopened and read.
    let written = fs::ensure_dir(ws, "made").and_then(|made| {
        let options = FileOpenOptions {
            read: false,
            write: true,
            append: false,
            create: true,
            truncate: true,
        };
        let file = fs::open_file(&made, "note.txt", options)?;
        file.write_string("probe", false)?;
        file.close();
        let again = fs::open_file(&made, "note.txt", read_only())?;
        let text = again.read_to_string(1024)?;
        again.close();
        made.close();
        Ok(text)
    });
    out.insert(
        "write_read".into(),
        json!(written.unwrap_or_else(|e| e.message)),
    );

    // `escape` is a symlink to a directory outside the workspace.
    let escape_dir = fs::open_dir(ws, "escape");
    out.insert("escape_dir".into(), verdict(&escape_dir));
    let escape_file = fs::open_file(ws, "escape/secret.txt", read_only());
    out.insert("escape_file".into(), verdict(&escape_file));

    // `big.bin` is 2 MiB; an unbounded read must come back capped.
    let huge = fs::open_file(ws, "big.bin", read_only()).and_then(|f| f.read(u64::MAX));
    out.insert(
        "huge_read".into(),
        json!(huge.map(|b| b.len()).unwrap_or_default()),
    );

    let echo = program("echo");
    let spawned = proc::spawn(&echo, &spawn_options(&["hi"], Vec::new(), None)).and_then(|p| {
        let read = p.read_stdout(1024)?;
        p.close();
        Ok(String::from_utf8_lossy(&read.data).into_owned())
    });
    out.insert(
        "spawn_echo".into(),
        json!(spawned.unwrap_or_else(|e| e.message)),
    );

    let loader = vec![EnvVar {
        key: "LD_PRELOAD".into(),
        value: "/tmp/x.so".into(),
    }];
    let ld = proc::spawn(&echo, &spawn_options(&[], loader, None));
    out.insert("spawn_ld".into(), verdict(&ld));

    let planted = proc::spawn(&program("planted"), &spawn_options(&[], Vec::new(), None));
    out.insert("spawn_planted".into(), verdict(&planted));

    let started = Instant::now();
    let timed = proc::spawn(
        &program("sleep"),
        &spawn_options(&["30"], Vec::new(), Some(200)),
    )
    .and_then(|p| p.wait(None));
    // The environment a spawned process sees: the host's PATH and nothing else.
    let seen_env =
        proc::spawn(&program("env"), &spawn_options(&[], Vec::new(), None)).and_then(|p| {
            let read = p.read_stdout(64 * 1024)?;
            p.close();
            Ok(String::from_utf8_lossy(&read.data).into_owned())
        });
    out.insert(
        "spawn_env".into(),
        json!(seen_env.unwrap_or_else(|e| e.message)),
    );

    // Every other way through the symlink out of the workspace.
    let escape_ensure = fs::ensure_dir(ws, "escape/new");
    out.insert("escape_ensure".into(), verdict(&escape_ensure));
    let escape_meta = fs::metadata(ws, Some("escape/secret.txt"));
    out.insert("escape_metadata".into(), verdict(&escape_meta));
    let escape_remove = fs::remove_file(ws, "escape/secret.txt");
    out.insert("escape_remove_file".into(), verdict(&escape_remove));
    let escape_remove_dir = fs::remove_dir(ws, "escape/sub", true);
    out.insert("escape_remove_dir".into(), verdict(&escape_remove_dir));
    let escape_rename = fs::rename(ws, "seed.txt", "escape/stolen.txt");
    out.insert("escape_rename".into(), verdict(&escape_rename));
    // And the other way: an entry outside moved in.
    let rename_in = fs::rename(ws, "../outside/secret.txt", "stolen-in.txt");
    out.insert("escape_rename_in".into(), verdict(&rename_in));
    let mut in_escape = spawn_options(&["x"], Vec::new(), None);
    in_escape.working_dir = Some("escape".into());
    let escape_cwd = proc::spawn(&echo, &in_escape);
    out.insert("escape_working_dir".into(), verdict(&escape_cwd));
    let path_env = vec![EnvVar {
        key: "PATH".into(),
        value: "/tmp".into(),
    }];
    let spawn_path = proc::spawn(&echo, &spawn_options(&[], path_env, None));
    out.insert("spawn_path".into(), verdict(&spawn_path));

    out.insert(
        "spawn_timeout".into(),
        json!({
            "timed_out": timed.as_ref().map(|s| s.timed_out).unwrap_or(false),
            "error": timed.err().map(|e| e.message),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
    // A stale handle: a directory opened, then swapped for a symlink out of the workspace using
    // only remove and rename (which act on the entry, as they should).
    let stale = fs::ensure_dir(ws, "h").and_then(|h| {
        fs::remove_dir(ws, "h", false)?;
        fs::rename(ws, "swap-out", "h")?;
        Ok(h)
    });
    match stale {
        Ok(h) => {
            out.insert("stale_list".into(), verdict(&fs::list_dir(&h)));
            out.insert("stale_metadata".into(), verdict(&fs::metadata(&h, None)));
        }
        Err(e) => {
            out.insert("stale_setup".into(), json!(e.message));
        }
    }
    // The same with a relative `..` link that pointed inside the workspace until it was moved up.
    let up = fs::ensure_dir(ws, "h2").and_then(|h| {
        fs::remove_dir(ws, "h2", false)?;
        fs::rename(ws, "a/up", "h2")?;
        Ok(h)
    });
    match up {
        Ok(h) => {
            out.insert("stale_up_list".into(), verdict(&fs::list_dir(&h)));
        }
        Err(e) => {
            out.insert("stale_up_setup".into(), json!(e.message));
        }
    }
    out
}

impl planner::Guest for Probe {
    fn step(task: String, _observation: planner::Observation) -> Result<StepResponse, AgentError> {
        let fail = |message: String| AgentError {
            retryable: false,
            message,
        };
        let ws = fs::open_workspace().map_err(|e| fail(e.message))?;
        let results = Value::Object(probe(&task, &ws)).to_string();
        let options = FileOpenOptions {
            read: false,
            write: true,
            append: false,
            create: true,
            truncate: true,
        };
        let file = fs::open_file(&ws, "results.json", options).map_err(|e| fail(e.message))?;
        file.write_string(&results, false)
            .map_err(|e| fail(e.message))?;
        file.close();
        ws.close();
        Ok(StepResponse::Complete(CompletePlan {
            reason: "probed".into(),
            outcome: "see results.json".into(),
        }))
    }
}

bindings::export!(Probe);
