use std::collections::HashMap;
use std::fs;
use std::io::{Read, Take};
use std::path::{Component, Path};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as Base64};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thirtyfour::prelude::*;
use tokio::runtime::Handle;

use crate::bindings::exports::osagent::agent::planner::PlannedAction;
use crate::config::{BrowserSettings, HostConfig};

#[derive(Debug, Serialize)]
pub struct ActionReport {
    pub capability: String,
    pub success: bool,
    pub output: Value,
    pub error: Option<String>,
}

impl ActionReport {
    fn succeeded(capability: String, output: Value) -> Self {
        Self {
            capability,
            success: true,
            output,
            error: None,
        }
    }

    fn failed(capability: String, err: anyhow::Error) -> Self {
        Self {
            capability,
            success: false,
            output: Value::Null,
            error: Some(err.to_string()),
        }
    }
}

/// Runs the planner's actions. Browser actions drive WebDriver futures with `Handle::block_on`, so
/// an executor lives on a blocking thread (`runtime::run_step`), never on a runtime worker.
pub struct ActionExecutor {
    config: HostConfig,
    tokio: Handle,
    browser_sessions: HashMap<String, BrowserSessionEntry>,
    browser_elements: HashMap<String, BrowserElementEntry>,
}

struct BrowserSessionEntry {
    driver: WebDriver,
    #[allow(dead_code)]
    profile: Option<String>,
}

struct BrowserElementEntry {
    element: WebElement,
    #[allow(dead_code)]
    session: String,
}

impl ActionExecutor {
    pub fn new(config: HostConfig, tokio: Handle) -> Self {
        Self {
            config,
            tokio,
            browser_sessions: HashMap::new(),
            browser_elements: HashMap::new(),
        }
    }

    pub fn execute(&mut self, actions: &[PlannedAction]) -> Vec<ActionReport> {
        actions
            .iter()
            .map(|action| self.execute_action(action))
            .collect()
    }

    fn execute_action(&mut self, action: &PlannedAction) -> ActionReport {
        let capability = action.capability.clone();
        let result = self.execute_action_inner(action);
        match result {
            Ok(value) => ActionReport::succeeded(capability, value),
            Err(err) => ActionReport::failed(capability, err),
        }
    }

    fn execute_action_inner(&mut self, action: &PlannedAction) -> Result<Value> {
        let input: Value = serde_json::from_str(&action.input).with_context(|| {
            format!("capability `{}` input is not valid JSON", action.capability)
        })?;
        match action.capability.as_str() {
            "fs.list_dir" => {
                let params: FsListDirInput = serde_json::from_value(input)?;
                self.fs_list_dir(params)
            }
            "fs.read_file" => {
                let params: FsReadFileInput = serde_json::from_value(input)?;
                self.fs_read_file(params)
            }
            "proc.spawn" => {
                let params: ProcSpawnInput = serde_json::from_value(input)?;
                self.proc_spawn(params)
            }
            "browser.open_session" => {
                let params: BrowserOpenSessionInput = serde_json::from_value(input)?;
                self.browser_open_session(params)
            }
            "browser.session.goto" => {
                let params: BrowserGotoInput = serde_json::from_value(input)?;
                self.browser_session_goto(params)
            }
            "browser.session.describe_page" => {
                let params: BrowserDescribeInput = serde_json::from_value(input)?;
                self.browser_session_describe(params)
            }
            "browser.session.find" => {
                let params: BrowserFindInput = serde_json::from_value(input)?;
                self.browser_session_find(params)
            }
            "browser.element.click" => {
                let params: BrowserElementActionInput = serde_json::from_value(input)?;
                self.browser_element_click(params)
            }
            "browser.element.type_text" => {
                let params: BrowserElementTypeInput = serde_json::from_value(input)?;
                self.browser_element_type(params)
            }
            "browser.element.inner_text" => {
                let params: BrowserElementActionInput = serde_json::from_value(input)?;
                self.browser_element_inner_text(params)
            }
            "browser.session.screenshot" => {
                let params: BrowserScreenshotInput = serde_json::from_value(input)?;
                self.browser_session_screenshot(params)
            }
            _ => Err(anyhow!("unsupported capability `{}`", action.capability)),
        }
    }

    fn fs_list_dir(&self, params: FsListDirInput) -> Result<Value> {
        let target = if let Some(path) = params.path {
            if path.trim().is_empty() {
                self.config.workspace_root.clone()
            } else {
                resolve_workspace_child(&self.config.workspace_root, &path)?
            }
        } else {
            self.config.workspace_root.clone()
        };
        let mut entries = Vec::new();
        let dir_iter = fs::read_dir(target.as_std_path())
            .with_context(|| format!("failed to list directory {}", target))?;
        for entry in dir_iter {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("entry name is not valid UTF-8"))?;
            entries.push(json!({
                "name": name,
                "kind": entry_kind(&metadata),
                "size_bytes": metadata.len(),
                "modified_ms": file_time_ms(&metadata),
            }));
        }
        Ok(json!({
            "path": target.as_str(),
            "entries": entries,
        }))
    }

    fn fs_read_file(&self, params: FsReadFileInput) -> Result<Value> {
        if params.path.trim().is_empty() {
            bail!("fs.read_file requires a non-empty `path`");
        }
        let target = resolve_workspace_child(&self.config.workspace_root, &params.path)?;
        let limit = params.max_bytes.unwrap_or(4096);
        let mut file = std::fs::File::open(target.as_std_path())
            .with_context(|| format!("failed to open file {}", target))?;
        let mut reader: Take<&mut std::fs::File> = (&mut file).take(limit + 1);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        let truncated = buffer.len() as u64 > limit;
        if truncated {
            buffer.truncate(limit as usize);
        }
        let (encoding, contents) = match String::from_utf8(buffer.clone()) {
            Ok(text) => ("utf-8", text),
            Err(_) => ("base64", Base64.encode(&buffer)),
        };
        Ok(json!({
            "path": target.as_str(),
            "truncated": truncated,
            "encoding": encoding,
            "contents": contents,
        }))
    }

    fn proc_spawn(&self, params: ProcSpawnInput) -> Result<Value> {
        if params.command.trim().is_empty() {
            bail!("proc.spawn requires `command`");
        }
        if !self.config.is_proc_allowed(&params.command) {
            bail!("command `{}` is not allowed by policy", params.command);
        }

        let working_dir = if let Some(cwd) = params.cwd {
            if cwd.trim().is_empty() {
                self.config.workspace_root.clone()
            } else {
                resolve_workspace_child(&self.config.workspace_root, &cwd)?
            }
        } else {
            self.config.workspace_root.clone()
        };

        let mut cmd = Command::new(&params.command);
        cmd.args(&params.args);
        cmd.current_dir(working_dir.as_std_path());
        cmd.env_clear();
        if let Some(env) = params.env {
            for var in env {
                cmd.env(var.key, var.value);
            }
        }

        let output = cmd
            .output()
            .with_context(|| format!("failed to execute {}", params.command))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok(json!({
            "command": params.command,
            "args": params.args,
            "cwd": working_dir.as_str(),
            "status": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        }))
    }

    fn browser_open_session(&mut self, params: BrowserOpenSessionInput) -> Result<Value> {
        let settings = self.browser_settings()?;
        let alias = normalized_alias(&params.alias)?;
        if self.browser_sessions.contains_key(&alias) {
            bail!("browser session `{alias}` already exists");
        }
        let webdriver_url = settings.webdriver_url.clone();
        let headless = params.headless.unwrap_or(true);
        let profile = params.profile.or_else(|| settings.default_profile.clone());
        let allow_downloads = params.allow_downloads.unwrap_or(false);
        let handle = self.tokio.clone();
        let driver = handle.block_on(async move {
            let mut caps = DesiredCapabilities::chrome();
            if headless {
                caps.add_arg("--headless=new")?;
                caps.add_arg("--disable-gpu")?;
            }
            caps.add_arg("--disable-dev-shm-usage")?;
            caps.add_arg("--no-sandbox")?;
            if allow_downloads {
                let prefs = serde_json::json!({
                    "download.prompt_for_download": false,
                });
                caps.add_experimental_option("prefs", prefs)?;
            }
            WebDriver::new(&webdriver_url, caps).await
        })?;

        self.browser_sessions
            .insert(alias.clone(), BrowserSessionEntry { driver, profile });
        Ok(json!({ "session": alias }))
    }

    fn browser_session_goto(&self, params: BrowserGotoInput) -> Result<Value> {
        let alias = normalized_alias(&params.session)?;
        let driver = self.session_driver(&alias)?;
        let url = params.url.clone();
        let timeout = params.timeout_ms.unwrap_or(5_000);
        self.tokio.block_on({
            let driver = driver.clone();
            async move {
                driver.goto(&url).await?;
                tokio::time::sleep(Duration::from_millis(timeout.min(30_000))).await;
                Ok::<_, WebDriverError>(())
            }
        })?;
        let current_url = self.tokio.block_on({
            let driver = driver.clone();
            async move { driver.current_url().await.map(|u| u.to_string()) }
        })?;
        Ok(json!({
            "session": alias,
            "url": current_url,
        }))
    }

    fn browser_session_describe(&self, params: BrowserDescribeInput) -> Result<Value> {
        let alias = normalized_alias(&params.session)?;
        let include_html = params.include_html.unwrap_or(false);
        let driver = self.session_driver(&alias)?;
        let driver_for_meta = driver.clone();
        let (url, title) = self.tokio.block_on(async move {
            let url = driver_for_meta.current_url().await?.to_string();
            let title = driver_for_meta.title().await.ok();
            Ok::<_, WebDriverError>((url, title))
        })?;
        let html = if include_html {
            let driver = driver.clone();
            Some(self.tokio.block_on(async move { driver.source().await })?)
        } else {
            None
        };
        Ok(json!({
            "session": alias,
            "url": url,
            "title": title,
            "html": html,
        }))
    }

    fn browser_session_find(&mut self, params: BrowserFindInput) -> Result<Value> {
        let session_alias = normalized_alias(&params.session)?;
        let element_alias = normalized_alias(&params.alias)?;
        if self.browser_elements.contains_key(&element_alias) {
            bail!("browser element `{element_alias}` already exists");
        }
        let driver = self.session_driver(&session_alias)?;
        let selector = selector_to_by(&params.selector)?;
        let timeout = params.timeout_ms.unwrap_or(5_000);
        let element = self.tokio.block_on(async move {
            let mut query = driver.query(selector);
            query = query.wait(Duration::from_millis(timeout), Duration::from_millis(200));
            query.first().await
        })?;
        self.browser_elements.insert(
            element_alias.clone(),
            BrowserElementEntry {
                element,
                session: session_alias.clone(),
            },
        );
        Ok(json!({
            "session": session_alias,
            "element": element_alias,
        }))
    }

    fn browser_element_click(&self, params: BrowserElementActionInput) -> Result<Value> {
        let element_alias = normalized_alias(&params.element)?;
        let element = self.element_handle(&element_alias)?;
        self.tokio.block_on(async move { element.click().await })?;
        Ok(json!({ "element": element_alias }))
    }

    fn browser_element_type(&self, params: BrowserElementTypeInput) -> Result<Value> {
        let element_alias = normalized_alias(&params.element)?;
        let element = self.element_handle(&element_alias)?;
        let text = params.text.unwrap_or_default();
        self.tokio
            .block_on(async move { element.send_keys(text).await })?;
        if params.submit.unwrap_or(false) {
            let element = self.element_handle(&element_alias)?;
            self.tokio
                .block_on(async move { element.send_keys(Key::Enter).await })?;
        }
        Ok(json!({ "element": element_alias }))
    }

    fn browser_element_inner_text(&self, params: BrowserElementActionInput) -> Result<Value> {
        let element_alias = normalized_alias(&params.element)?;
        let element = self.element_handle(&element_alias)?;
        let text = self.tokio.block_on(async move { element.text().await })?;
        Ok(json!({
            "element": element_alias,
            "text": text,
        }))
    }

    fn browser_session_screenshot(&self, params: BrowserScreenshotInput) -> Result<Value> {
        let alias = normalized_alias(&params.session)?;
        let driver = self.session_driver(&alias)?;
        let raw = self
            .tokio
            .block_on(async move { driver.screenshot_as_png().await })?;
        let encoded = Base64.encode(raw);
        Ok(json!({
            "session": alias,
            "kind": params.kind.unwrap_or(ScreenshotKind::Png),
            "data_base64": encoded,
        }))
    }

    fn browser_settings(&self) -> Result<&BrowserSettings> {
        self.config
            .browser
            .as_ref()
            .ok_or_else(|| anyhow!("browser capability is disabled in host configuration"))
    }

    fn session_driver(&self, alias: &str) -> Result<WebDriver> {
        self.browser_sessions
            .get(alias)
            .map(|entry| entry.driver.clone())
            .ok_or_else(|| anyhow!("unknown browser session `{alias}`"))
    }

    fn element_handle(&self, alias: &str) -> Result<WebElement> {
        self.browser_elements
            .get(alias)
            .map(|entry| entry.element.clone())
            .ok_or_else(|| anyhow!("unknown browser element `{alias}`"))
    }
}

impl Drop for ActionExecutor {
    fn drop(&mut self) {
        let handle = self.tokio.clone();
        for (_, entry) in self.browser_sessions.drain() {
            let driver = entry.driver.clone();
            let _ = handle.block_on(async move { driver.quit().await });
        }
        self.browser_elements.clear();
    }
}

#[derive(Deserialize)]
struct FsListDirInput {
    path: Option<String>,
}

#[derive(Deserialize)]
struct FsReadFileInput {
    path: String,
    max_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct ProcSpawnInput {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<ProcEnvVar>>,
}

#[derive(Deserialize)]
struct ProcEnvVar {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct BrowserOpenSessionInput {
    alias: String,
    profile: Option<String>,
    headless: Option<bool>,
    allow_downloads: Option<bool>,
}

#[derive(Deserialize)]
struct BrowserGotoInput {
    session: String,
    url: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct BrowserDescribeInput {
    session: String,
    include_html: Option<bool>,
}

#[derive(Deserialize)]
struct BrowserFindInput {
    session: String,
    selector: BrowserSelector,
    timeout_ms: Option<u64>,
    alias: String,
}

#[derive(Deserialize)]
struct BrowserElementActionInput {
    element: String,
}

#[derive(Deserialize)]
struct BrowserElementTypeInput {
    element: String,
    text: Option<String>,
    submit: Option<bool>,
}

#[derive(Deserialize)]
struct BrowserScreenshotInput {
    session: String,
    kind: Option<ScreenshotKind>,
}

#[derive(Deserialize)]
struct BrowserSelector {
    kind: BrowserSelectorKind,
    value: String,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum BrowserSelectorKind {
    Css,
    XPath,
    Text,
}

#[derive(Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum ScreenshotKind {
    Png,
    Jpeg,
}

fn normalized_alias(input: &str) -> Result<String> {
    if input.trim().is_empty() {
        bail!("alias must be non-empty");
    }
    Ok(input.trim().to_string())
}

fn selector_to_by(selector: &BrowserSelector) -> Result<By> {
    match selector.kind {
        BrowserSelectorKind::Css => Ok(By::Css(selector.value.clone())),
        BrowserSelectorKind::XPath => Ok(By::XPath(selector.value.clone())),
        BrowserSelectorKind::Text => {
            let text_literal = serde_json::to_string(&selector.value)?;
            let xpath = format!("//*[normalize-space(text()) = {}]", text_literal);
            Ok(By::XPath(xpath))
        }
    }
}

fn resolve_workspace_child(root: &Utf8Path, relative: &str) -> Result<Utf8PathBuf> {
    if relative.is_empty() {
        return Ok(root.to_path_buf());
    }
    let rel_path = Path::new(relative);
    if rel_path.is_absolute() {
        bail!("absolute paths are not allowed");
    }
    let mut candidate = root.as_std_path().to_path_buf();
    for component in rel_path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(seg) => candidate.push(seg),
            _ => bail!("path traversal segments are not allowed"),
        }
    }
    let candidate =
        Utf8PathBuf::from_path_buf(candidate).map_err(|_| anyhow!("path is not valid UTF-8"))?;
    ensure_within_workspace(root, &candidate)?;
    Ok(candidate)
}

fn ensure_within_workspace(root: &Utf8Path, candidate: &Utf8Path) -> Result<()> {
    if candidate.as_std_path().starts_with(root.as_std_path()) {
        Ok(())
    } else {
        bail!("path `{}` escapes workspace root", candidate)
    }
}

fn entry_kind(meta: &fs::Metadata) -> &'static str {
    if meta.is_file() {
        "file"
    } else if meta.is_dir() {
        "directory"
    } else if meta.file_type().is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

fn file_time_ms(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp directory, canonicalised (as the host resolves the
    /// workspace root).
    fn scratch(name: &str) -> Utf8PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("hostd-unit-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        let dir = fs::canonicalize(dir).expect("canonical scratch dir");
        Utf8PathBuf::from_path_buf(dir).expect("UTF-8 scratch dir")
    }

    fn config(root: &Utf8Path, allow_proc: &[&str]) -> HostConfig {
        HostConfig {
            workspace_root: root.to_path_buf(),
            allowed_proc_commands: allow_proc.iter().map(|s| s.to_string()).collect(),
            llm: None,
            browser: None,
        }
    }

    /// Run one action through an executor over `root`; its report.
    fn run(root: &Utf8Path, allow_proc: &[&str], capability: &str, input: Value) -> ActionReport {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let mut executor = ActionExecutor::new(config(root, allow_proc), runtime.handle().clone());
        let action = PlannedAction {
            capability: capability.to_string(),
            input: input.to_string(),
            audit_tag: None,
        };
        executor.execute(&[action]).remove(0)
    }

    // ── Workspace path resolution ───────────────────────────────────────────

    #[test]
    fn an_empty_path_is_the_workspace_root() {
        let root = Utf8Path::new("/work/sandbox");
        assert_eq!(resolve_workspace_child(root, "").unwrap(), root);
    }

    #[test]
    fn a_relative_path_resolves_under_the_root() {
        let root = Utf8Path::new("/work/sandbox");
        assert_eq!(
            resolve_workspace_child(root, "src/main.rs").unwrap(),
            Utf8Path::new("/work/sandbox/src/main.rs")
        );
    }

    #[test]
    fn current_directory_segments_are_dropped() {
        let root = Utf8Path::new("/work/sandbox");
        assert_eq!(
            resolve_workspace_child(root, "./a/./b").unwrap(),
            Utf8Path::new("/work/sandbox/a/b")
        );
    }

    #[test]
    fn a_parent_segment_is_refused() {
        let root = Utf8Path::new("/work/sandbox");
        for path in ["../etc/passwd", "a/../../etc", "a/.."] {
            assert!(resolve_workspace_child(root, path).is_err(), "{path}");
        }
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let root = Utf8Path::new("/work/sandbox");
        assert!(resolve_workspace_child(root, "/etc/passwd").is_err());
    }

    /// `/work/sandbox2` shares a string prefix with `/work/sandbox` but is not inside it.
    #[test]
    fn a_sibling_sharing_the_roots_prefix_is_outside() {
        let root = Utf8Path::new("/work/sandbox");
        assert!(ensure_within_workspace(root, Utf8Path::new("/work/sandbox2/secret")).is_err());
        assert!(ensure_within_workspace(root, Utf8Path::new("/work/sandbox/inner")).is_ok());
    }

    /// Known gap (README, Status): the check is lexical, so a symlink inside the workspace that points
    /// out of it is followed.
    #[test]
    #[ignore = "known gap: the workspace check is lexical and follows symlinks out of the workspace"]
    fn a_symlink_out_of_the_workspace_is_refused() {
        let root = scratch("symlink");
        let outside = scratch("outside");
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("escape").as_std_path())
            .unwrap();
        let report = run(
            &root,
            &[],
            "fs.read_file",
            json!({ "path": "escape/secret.txt" }),
        );
        assert!(
            !report.success,
            "read through a symlink: {:?}",
            report.output
        );
    }

    // ── fs actions ──────────────────────────────────────────────────────────

    #[test]
    fn list_dir_lists_the_workspace() {
        let root = scratch("list");
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        let report = run(&root, &[], "fs.list_dir", json!({}));
        assert!(report.success, "{:?}", report.error);
        let mut entries: Vec<(String, String)> = report.output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [
                ("a.txt".to_string(), "file".to_string()),
                ("sub".to_string(), "directory".to_string())
            ]
        );
    }

    #[test]
    fn read_file_reads_within_max_bytes_and_flags_truncation() {
        let root = scratch("read");
        fs::write(root.join("notes.txt"), "hello world").unwrap();
        let whole = run(&root, &[], "fs.read_file", json!({ "path": "notes.txt" }));
        assert_eq!(whole.output["contents"], "hello world");
        assert_eq!(whole.output["truncated"], false);
        let part = run(
            &root,
            &[],
            "fs.read_file",
            json!({ "path": "notes.txt", "max_bytes": 5 }),
        );
        assert_eq!(part.output["contents"], "hello");
        assert_eq!(part.output["truncated"], true);
    }

    #[test]
    fn read_file_returns_base64_for_bytes_that_are_not_utf8() {
        let root = scratch("binary");
        fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let report = run(&root, &[], "fs.read_file", json!({ "path": "blob.bin" }));
        assert_eq!(report.output["encoding"], "base64");
        assert_eq!(report.output["contents"], Base64.encode([0xff, 0xfe, 0x00]));
    }

    #[test]
    fn read_file_refuses_an_empty_path() {
        let root = scratch("empty");
        let report = run(&root, &[], "fs.read_file", json!({ "path": " " }));
        assert!(!report.success);
    }

    #[test]
    fn an_unknown_capability_is_a_failed_report() {
        let root = scratch("unknown");
        let report = run(&root, &[], "fs.delete_everything", json!({}));
        assert!(!report.success);
        assert!(report.error.unwrap().contains("unsupported capability"));
    }

    #[test]
    fn input_that_does_not_match_the_capability_is_a_failed_report() {
        let root = scratch("badinput");
        let report = run(&root, &[], "fs.read_file", json!({ "file": "x" }));
        assert!(!report.success);
    }

    // ── proc actions ────────────────────────────────────────────────────────

    #[test]
    fn an_unlisted_command_is_refused() {
        let root = scratch("unlisted");
        let report = run(&root, &["echo"], "proc.spawn", json!({ "command": "pwd" }));
        assert!(!report.success);
        assert!(report.error.unwrap().contains("not allowed by policy"));
    }

    #[test]
    fn an_allowed_command_runs_in_the_workspace() {
        let root = scratch("allowed");
        let pwd = which("pwd");
        let report = run(
            &root,
            &[pwd.as_str()],
            "proc.spawn",
            json!({ "command": pwd }),
        );
        assert!(report.success, "{:?}", report.error);
        assert_eq!(report.output["status"], 0);
        assert_eq!(
            report.output["stdout"].as_str().unwrap().trim(),
            root.as_str()
        );
    }

    #[test]
    fn a_spawned_command_gets_only_the_environment_it_is_given() {
        let root = scratch("env");
        let env = which("env");
        let report = run(
            &root,
            &[env.as_str()],
            "proc.spawn",
            json!({ "command": env, "env": [{ "key": "ONLY", "value": "this" }] }),
        );
        assert!(report.success, "{:?}", report.error);
        assert_eq!(
            report.output["stdout"].as_str().unwrap().trim(),
            "ONLY=this"
        );
    }

    /// Known gap (README, Status): the allowlist matches a command's base name, so any program named
    /// like an allowed one runs.
    #[test]
    #[ignore = "known gap: the allowlist matches base names, so /any/dir/ls passes for `ls`"]
    fn the_allowlist_matches_the_resolved_program_not_its_base_name() {
        let root = scratch("basename");
        let fake = root.join("pwd");
        fs::write(&fake, "#!/bin/sh\necho planted\n").unwrap();
        let mut perms = fs::metadata(&fake).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&fake, perms).unwrap();
        let report = run(
            &root,
            &["pwd"],
            "proc.spawn",
            json!({ "command": fake.as_str() }),
        );
        assert!(!report.success, "a planted `pwd` ran: {:?}", report.output);
    }

    /// Known gap (README, Status): loader variables from the plan reach the spawned process.
    #[test]
    #[ignore = "known gap: plan-supplied LD_* variables reach the spawned process"]
    fn loader_variables_from_the_plan_are_dropped() {
        let root = scratch("ld");
        let env = which("env");
        let report = run(
            &root,
            &[env.as_str()],
            "proc.spawn",
            json!({ "command": env, "env": [{ "key": "LD_PRELOAD", "value": "/tmp/x.so" }] }),
        );
        let stdout = report.output["stdout"].as_str().unwrap_or_default();
        assert!(!stdout.contains("LD_PRELOAD"), "{stdout}");
    }

    /// The absolute path of `program` on this machine's PATH.
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

    // ── Browser actions, without a browser ──────────────────────────────────

    #[test]
    fn browser_actions_are_off_without_a_browser_section() {
        let root = scratch("nobrowser");
        let report = run(&root, &[], "browser.open_session", json!({ "alias": "s1" }));
        assert!(!report.success);
        assert!(report.error.unwrap().contains("disabled"));
    }
}
