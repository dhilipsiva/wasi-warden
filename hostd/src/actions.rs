use std::collections::HashMap;
use std::fs;
use std::io::{Read, Take};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as Base64};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thirtyfour::prelude::*;
use tokio::runtime::Handle;

use crate::bindings::exports::osagent::agent::planner::PlannedAction;
use crate::config::{BrowserSettings, HostConfig};
use crate::sandbox;

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
                self.confine(&path)?
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
        let target = self.confine(&params.path)?;
        // Capped, so `limit + 1` below cannot overflow.
        let limit = sandbox::cap_read(params.max_bytes.unwrap_or(4096));
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
        let Some(program) = self.config.program_for(&params.command) else {
            bail!("command `{}` is not allowed by policy", params.command);
        };
        if let Some(env) = &params.env {
            for var in env {
                sandbox::check_env_key(&var.key, &self.config.allowed_env)
                    .map_err(anyhow::Error::msg)?;
            }
        }

        let working_dir = if let Some(cwd) = params.cwd {
            if cwd.trim().is_empty() {
                self.config.workspace_root.clone()
            } else {
                self.confine(&cwd)?
            }
        } else {
            self.config.workspace_root.clone()
        };

        // The allowlist's resolved program, never a PATH lookup of the plan's string.
        let mut cmd = Command::new(program);
        cmd.args(&params.args);
        cmd.current_dir(working_dir.as_std_path());
        cmd.env_clear();
        // The host's PATH, outside the workspace; the plan cannot set it.
        cmd.env("PATH", &self.config.child_path);
        if let Some(env) = params.env {
            for var in env {
                cmd.env(var.key, var.value);
            }
        }

        let output = sandbox::run_bounded(cmd, sandbox::proc_timeout(params.timeout_ms))
            .with_context(|| format!("failed to execute {}", params.command))?;
        Ok(json!({
            "command": params.command,
            "args": params.args,
            "cwd": working_dir.as_str(),
            "status": output.status,
            "timed_out": output.timed_out,
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "stdout_truncated": output.stdout_truncated,
            "stderr_truncated": output.stderr_truncated,
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
        let no_sandbox = settings.chrome_no_sandbox;
        let download_dir = self.config.workspace_root.to_string();
        let handle = self.tokio.clone();
        let driver = handle.block_on(async move {
            let mut caps = DesiredCapabilities::chrome();
            if headless {
                caps.add_arg("--headless=new")?;
                caps.add_arg("--disable-gpu")?;
            }
            caps.add_arg("--disable-dev-shm-usage")?;
            if no_sandbox {
                caps.add_arg("--no-sandbox")?;
            }
            if allow_downloads {
                // Into the workspace, not the user's Downloads folder.
                let prefs = serde_json::json!({
                    "download.prompt_for_download": false,
                    "download.default_directory": download_dir,
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
        let allowed = &self.browser_settings()?.allowed_hosts;
        // The URL as parsed is what the browser gets, so the check and the visit agree.
        let url = sandbox::check_navigation(&params.url, allowed).map_err(anyhow::Error::msg)?;
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
        // A page may redirect anywhere; one that left the allowed hosts is abandoned.
        if let Err(why) = sandbox::check_navigation(&current_url, allowed) {
            self.tokio.block_on({
                let driver = driver.clone();
                async move { driver.goto("about:blank").await }
            })?;
            bail!("the page redirected off the allowed hosts ({why}); left for about:blank");
        }
        Ok(json!({
            "session": alias,
            "url": current_url,
        }))
    }

    fn browser_session_describe(&self, params: BrowserDescribeInput) -> Result<Value> {
        let alias = normalized_alias(&params.session)?;
        let include_html = params.include_html.unwrap_or(false);
        let driver = self.checked_driver(&alias)?;
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
        let driver = self.checked_driver(&session_alias)?;
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
        let element = self.checked_element(&element_alias)?;
        self.tokio.block_on(async move { element.click().await })?;
        Ok(json!({ "element": element_alias }))
    }

    fn browser_element_type(&self, params: BrowserElementTypeInput) -> Result<Value> {
        let element_alias = normalized_alias(&params.element)?;
        let element = self.checked_element(&element_alias)?;
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
        let element = self.checked_element(&element_alias)?;
        let text = self.tokio.block_on(async move { element.text().await })?;
        Ok(json!({
            "element": element_alias,
            "text": text,
        }))
    }

    fn browser_session_screenshot(&self, params: BrowserScreenshotInput) -> Result<Value> {
        let alias = normalized_alias(&params.session)?;
        let driver = self.checked_driver(&alias)?;
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

    /// The session's page must still be on the allowed hosts (or blank): a page can navigate itself
    /// (a script, a meta refresh, a followed link) after `goto` checked it. A page that wandered off
    /// is left for `about:blank` and the action refused.
    fn ensure_allowed_page(&self, driver: &WebDriver) -> Result<()> {
        let allowed = &self.browser_settings()?.allowed_hosts;
        let current = self.tokio.block_on({
            let driver = driver.clone();
            async move { driver.current_url().await.map(|u| u.to_string()) }
        })?;
        if current == "about:blank" {
            return Ok(());
        }
        if let Err(why) = sandbox::check_navigation(&current, allowed) {
            self.tokio.block_on({
                let driver = driver.clone();
                async move { driver.goto("about:blank").await }
            })?;
            bail!("the page left the allowed hosts ({why}); left for about:blank");
        }
        Ok(())
    }

    /// The driver of the session `alias`, after checking its page ([`Self::ensure_allowed_page`]).
    fn checked_driver(&self, alias: &str) -> Result<WebDriver> {
        let driver = self.session_driver(alias)?;
        self.ensure_allowed_page(&driver)?;
        Ok(driver)
    }

    /// The element `alias`, after checking its session's page ([`Self::ensure_allowed_page`]).
    fn checked_element(&self, alias: &str) -> Result<WebElement> {
        let session = self
            .browser_elements
            .get(alias)
            .map(|entry| entry.session.clone())
            .ok_or_else(|| anyhow!("unknown browser element `{alias}`"))?;
        self.ensure_allowed_page(&self.session_driver(&session)?)?;
        self.element_handle(alias)
    }

    /// `relative` under the workspace root ([`sandbox::confine`]).
    fn confine(&self, relative: &str) -> Result<Utf8PathBuf> {
        let root = &self.config.workspace_root;
        Ok(sandbox::confine(root, root, relative)?)
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
    /// Capped by the host ([`sandbox::proc_timeout`]); the default applies when unset.
    timeout_ms: Option<u64>,
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
    use camino::Utf8Path;

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
        let path_var = std::env::var_os("PATH");
        HostConfig {
            workspace_root: root.to_path_buf(),
            allowed_proc_commands: allow_proc.iter().map(|s| s.to_string()).collect(),
            allowed_programs: allow_proc
                .iter()
                .map(|s| sandbox::resolve_allowed(s, path_var.as_deref(), root).expect("resolves"))
                .collect(),
            allowed_env: vec!["ONLY".to_string()],
            child_path: sandbox::child_path(path_var.as_deref(), root),
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

    // ── Workspace paths (the rules themselves are tested in sandbox.rs) ──────

    /// A symlink inside the workspace that points out of it is not followed.
    #[test]
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
        let listed = run(&root, &[], "fs.list_dir", json!({ "path": "escape" }));
        assert!(
            !listed.success,
            "listed through a symlink: {:?}",
            listed.output
        );
    }

    /// A dangling symlink cannot be checked, so it is refused (a later target could be anywhere).
    #[test]
    fn a_dangling_symlink_is_refused() {
        let root = scratch("dangling");
        let outside = scratch("gone");
        std::os::unix::fs::symlink(
            outside.join("not-yet").as_std_path(),
            root.join("later").as_std_path(),
        )
        .unwrap();
        let report = run(&root, &[], "fs.read_file", json!({ "path": "later" }));
        assert!(!report.success);
    }

    /// A symlink that stays inside the workspace is followed.
    #[test]
    fn a_symlink_inside_the_workspace_is_followed() {
        let root = scratch("inside");
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/file.txt"), "inside").unwrap();
        std::os::unix::fs::symlink(
            root.join("real").as_std_path(),
            root.join("alias").as_std_path(),
        )
        .unwrap();
        let report = run(
            &root,
            &[],
            "fs.read_file",
            json!({ "path": "alias/file.txt" }),
        );
        assert_eq!(report.output["contents"], "inside", "{:?}", report.error);
    }

    /// An unbounded `max_bytes` is capped at 1 MiB, not overflowed.
    #[test]
    fn an_unbounded_max_bytes_is_capped() {
        let root = scratch("maxbytes");
        fs::write(root.join("big.bin"), vec![b'a'; (1 << 20) + 10]).unwrap();
        let report = run(
            &root,
            &[],
            "fs.read_file",
            json!({ "path": "big.bin", "max_bytes": u64::MAX }),
        );
        assert!(report.success, "{:?}", report.error);
        assert_eq!(report.output["contents"].as_str().unwrap().len(), 1 << 20);
        assert_eq!(report.output["truncated"], true);
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
        let mut lines: Vec<String> = report.output["stdout"]
            .as_str()
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        let path = sandbox::child_path(std::env::var_os("PATH").as_deref(), &root);
        assert_eq!(
            lines,
            [
                "ONLY=this".to_string(),
                format!("PATH={}", path.to_string_lossy())
            ],
            "the allowed variable, and the host's PATH"
        );
        let unlisted = run(
            &root,
            &[env.as_str()],
            "proc.spawn",
            json!({ "command": env, "env": [{ "key": "HOME", "value": "/root" }] }),
        );
        assert!(!unlisted.success, "a variable off allow_env is refused");
    }

    /// The allowlist names programs, not base names: a program planted elsewhere under an allowed
    /// name does not run.
    #[test]
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

    /// Loader variables from the plan are refused: they would let the plan inject code into an
    /// allowed program.
    #[test]
    fn loader_variables_from_the_plan_are_refused() {
        let root = scratch("ld");
        let env = which("env");
        let report = run(
            &root,
            &[env.as_str()],
            "proc.spawn",
            json!({ "command": env, "env": [{ "key": "LD_PRELOAD", "value": "/tmp/x.so" }] }),
        );
        assert!(!report.success, "LD_PRELOAD passed: {:?}", report.output);
        let dyld = run(
            &root,
            &[env.as_str()],
            "proc.spawn",
            json!({ "command": env, "env": [{ "key": "DYLD_INSERT_LIBRARIES", "value": "x" }] }),
        );
        assert!(!dyld.success);
    }

    #[test]
    fn a_command_past_its_timeout_is_killed() {
        let root = scratch("timeout");
        let sleep = which("sleep");
        let started = std::time::Instant::now();
        let report = run(
            &root,
            &[sleep.as_str()],
            "proc.spawn",
            json!({ "command": sleep, "args": ["30"], "timeout_ms": 200 }),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed promptly"
        );
        assert_eq!(
            report.output["timed_out"], true,
            "{:?} {:?}",
            report.output, report.error
        );
    }

    /// The timeout kills the whole process group, so a child the command started cannot hold the
    /// step open.
    #[test]
    fn a_timeout_kills_the_commands_children_too() {
        let root = scratch("group");
        let sh = which("sh");
        // The script names `sleep` in full so it does not depend on the child's PATH.
        let script = format!("{0} 30 & echo $!; {0} 30", which("sleep"));
        let started = std::time::Instant::now();
        let report = run(
            &root,
            &[sh.as_str()],
            "proc.spawn",
            json!({ "command": sh, "args": ["-c", script], "timeout_ms": 200 }),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the group was killed"
        );
        assert_eq!(report.output["timed_out"], true);
        assert!(
            gone(&report.output["stdout"]),
            "the background child is dead"
        );
    }

    /// A command's background children end with it, even ones that dropped their output.
    #[test]
    fn a_commands_background_children_end_with_it() {
        let root = scratch("background");
        let sh = which("sh");
        let script = format!("{} 30 >/dev/null 2>&1 & echo $!", which("sleep"));
        let started = std::time::Instant::now();
        let report = run(
            &root,
            &[sh.as_str()],
            "proc.spawn",
            json!({ "command": sh, "args": ["-c", script] }),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report.output["status"], 0, "{:?}", report.error);
        assert!(
            gone(&report.output["stdout"]),
            "the background child is dead"
        );
    }

    /// The pid printed on the first line of `stdout` names a process that is gone (or a zombie)
    /// within two seconds.
    fn gone(stdout: &Value) -> bool {
        let pid: u32 = stdout
            .as_str()
            .and_then(|s| s.lines().next())
            .and_then(|l| l.trim().parse().ok())
            .expect("a pid on the first line");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) => {
                    let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
                    if state.is_some_and(|r| r.starts_with('Z')) {
                        return true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Variables that let a plan steer what an allowed program runs (its PATH, shell hooks,
    /// interpreter search paths) are refused like the loader's.
    #[test]
    fn the_plan_cannot_set_path_or_shell_hooks() {
        let root = scratch("hooks");
        let env = which("env");
        for key in [
            "PATH",
            "BASH_FUNC_ls%%",
            "BASH_ENV",
            "ENV",
            "GCONV_PATH",
            "GLIBC_TUNABLES",
            "NODE_OPTIONS",
            "PYTHONPATH",
        ] {
            let report = run(
                &root,
                &[env.as_str()],
                "proc.spawn",
                json!({ "command": env, "env": [{ "key": key, "value": "x" }] }),
            );
            assert!(!report.success, "{key} passed: {:?}", report.output);
        }
    }

    #[test]
    fn a_working_directory_through_a_symlink_out_is_refused() {
        let root = scratch("cwd");
        let outside = scratch("cwd-outside");
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("escape").as_std_path())
            .unwrap();
        let pwd = which("pwd");
        let report = run(
            &root,
            &[pwd.as_str()],
            "proc.spawn",
            json!({ "command": pwd, "cwd": "escape" }),
        );
        assert!(!report.success, "ran in {:?}", report.output["cwd"]);
    }

    #[test]
    fn a_commands_output_is_capped() {
        let root = scratch("cap");
        let seq = which("seq");
        let report = run(
            &root,
            &[seq.as_str()],
            "proc.spawn",
            json!({ "command": seq, "args": ["1", "1000000"] }),
        );
        assert!(report.success, "{:?}", report.error);
        assert_eq!(report.output["status"], 0);
        assert_eq!(report.output["stdout"].as_str().unwrap().len(), 1 << 20);
        assert_eq!(report.output["stdout_truncated"], true);
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
