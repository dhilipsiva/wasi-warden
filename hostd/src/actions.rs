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
