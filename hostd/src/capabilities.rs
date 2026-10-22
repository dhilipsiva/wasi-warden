use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::Agent;
use wasmtime::component::{Resource, ResourceTableError};

use crate::bindings;
use crate::browser::{self, Browser, Select};

/// The largest event (its type and payload) or grant request the agent may log.
const MAX_EVENT_BYTES: usize = 4096;
/// The most events and grant requests the agent may log in a run.
const MAX_EVENTS: u32 = 1000;
/// The longest one LLM request may take.
const LLM_TIMEOUT: Duration = Duration::from_secs(60);
use crate::budget::{self, Kind};
use crate::config::{HostConfig, LlmSettings};
use crate::resources::{
    BrowserElementResource, BrowserSessionResource, DirHandleResource, FileHandleResource,
    ProcessResource,
};
use crate::sandbox;
use crate::secrets::Secrets;
use crate::state::HostState;
use bindings::osagent::llm::llm::Role as MessageRole;

type CapabilityError = bindings::osagent::common::types::CapabilityError;
type CapabilityErrorCode = bindings::osagent::common::types::CapabilityErrorCode;
type DirHandle = bindings::osagent::fs::fs::DirHandle;
type FileHandle = bindings::osagent::fs::fs::FileHandle;
type ProcHandle = bindings::osagent::proc::proc::Process;

fn capability_error(code: CapabilityErrorCode, message: impl Into<String>) -> CapabilityError {
    CapabilityError {
        code,
        message: message.into(),
        detail: None,
    }
}

fn table_error(err: ResourceTableError) -> CapabilityError {
    match err {
        ResourceTableError::NotPresent | ResourceTableError::WrongType => {
            capability_error(CapabilityErrorCode::InvalidArgument, "invalid handle")
        }
        ResourceTableError::Full => capability_error(
            CapabilityErrorCode::Limit,
            "too many open capability handles",
        ),
        ResourceTableError::HasChildren => {
            capability_error(CapabilityErrorCode::Conflict, "resource has live children")
        }
    }
}

fn require_llm_settings(config: &HostConfig) -> Result<&LlmSettings, CapabilityError> {
    config.llm.as_ref().ok_or_else(|| {
        capability_error(
            CapabilityErrorCode::Unavailable,
            "llm capability is not configured",
        )
    })
}

fn http_agent(timeout: Duration) -> Agent {
    ureq::AgentBuilder::new().timeout(timeout).build()
}

fn chat_endpoint(base: &str) -> String {
    format!("{}/chat/completions", base.trim_end_matches('/'))
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction,
}

#[derive(Serialize)]
struct ChatToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(rename = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Deserialize, Serialize)]
struct ToolCall {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    function: FunctionCall,
}

#[derive(Deserialize, Serialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

fn convert_usage(usage: Option<ChatUsage>) -> Option<bindings::osagent::llm::llm::TokenUsage> {
    usage.map(|u| {
        let prompt = u.prompt_tokens.unwrap_or_default();
        let completion = u.completion_tokens.unwrap_or_default();
        bindings::osagent::llm::llm::TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: u
                .total_tokens
                .unwrap_or_else(|| prompt.saturating_add(completion)),
        }
    })
}

/// The tokens a call is charged: the endpoint's total, or its prompt and completion counts, or, when
/// it reports neither, an estimate of a token per four bytes of the request and the reply. An
/// endpoint that reports nothing cannot switch the budget off.
fn tokens_charged(usage: Option<&ChatUsage>, request: &ChatRequest, reply_bytes: usize) -> u64 {
    let reported =
        usage.and_then(
            |u| match (u.total_tokens, u.prompt_tokens, u.completion_tokens) {
                (Some(total), _, _) if total > 0 => Some(u64::from(total)),
                (_, None, None) => None,
                (_, prompt, completion) => {
                    Some(u64::from(prompt.unwrap_or(0)) + u64::from(completion.unwrap_or(0)))
                }
            },
        );
    reported.filter(|t| *t > 0).unwrap_or_else(|| {
        let request_bytes = serde_json::to_vec(request).map(|v| v.len()).unwrap_or(0);
        ((request_bytes + reply_bytes) as u64).div_ceil(4)
    })
}

/// The guest's `max_tokens`, never more than the token budget has left.
fn clamp_max_tokens(requested: Option<u32>, left: u64) -> Option<u32> {
    requested.map(|m| m.min(u32::try_from(left).unwrap_or(u32::MAX)))
}

fn messages_to_chat(
    messages: Vec<bindings::osagent::llm::llm::Message>,
    secrets: &Secrets,
) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .map(|msg| ChatMessage {
            role: match msg.role {
                MessageRole::System => "system".to_string(),
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
                MessageRole::Tool => "tool".to_string(),
            },
            content: secrets.redact(&msg.content),
            name: msg.name.filter(|n| !n.trim().is_empty()),
        })
        .collect()
}

fn tools_to_chat(
    tools: Vec<bindings::osagent::llm::llm::ToolSchema>,
    secrets: &Secrets,
) -> Result<Vec<ChatTool>, CapabilityError> {
    let mut result = Vec::with_capacity(tools.len());
    for tool in tools {
        let parameters: Value = serde_json::from_str(&tool.schema_json).map_err(|_| {
            capability_error(
                CapabilityErrorCode::InvalidArgument,
                format!("invalid tool schema for {}", tool.name),
            )
        })?;
        result.push(ChatTool {
            kind: "function",
            function: ChatToolFunction {
                name: secrets.redact(&tool.name),
                description: Some(secrets.redact(&tool.description)),
                parameters: secrets.redact_value(&parameters),
            },
        });
    }
    Ok(result)
}

/// The request for the model. Every message is redacted here, the one place everything bound for the
/// model passes: no secret value reaches it, whatever the guest puts in a message.
fn build_chat_request(
    model: &str,
    messages: Vec<bindings::osagent::llm::llm::Message>,
    options: bindings::osagent::llm::llm::Options,
    tools: Option<Vec<ChatTool>>,
    secrets: &Secrets,
) -> ChatRequest {
    let stop = if options.stop.is_empty() {
        None
    } else {
        Some(options.stop)
    };
    ChatRequest {
        model: model.to_string(),
        messages: messages_to_chat(messages, secrets),
        tool_choice: tools.as_ref().map(|_| "auto".to_string()),
        tools,
        max_tokens: options.max_tokens,
        temperature: options.temperature,
        top_p: options.top_p,
        stop,
        presence_penalty: options.presence_penalty,
        frequency_penalty: options.frequency_penalty,
    }
}

fn execute_chat_request(
    settings: &LlmSettings,
    body: &ChatRequest,
    timeout: Duration,
) -> Result<ChatResponse, CapabilityError> {
    let url = chat_endpoint(&settings.api_base);
    let agent = http_agent(timeout);
    let token = format!("Bearer {}", settings.api_key);
    let payload = serde_json::to_value(body).map_err(|err| {
        capability_error(
            CapabilityErrorCode::Internal,
            format!("failed to encode llm request: {err}"),
        )
    })?;
    let response = agent
        .post(&url)
        .set("Authorization", &token)
        .set("Content-Type", "application/json")
        .send_json(payload);
    let response = response.map_err(|err| match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            capability_error(
                CapabilityErrorCode::Unavailable,
                format!("llm error {code}: {body}"),
            )
        }
        ureq::Error::Transport(tr) => capability_error(
            CapabilityErrorCode::Unavailable,
            format!("llm transport error: {tr}"),
        ),
    })?;
    response.into_json().map_err(|err| {
        capability_error(
            CapabilityErrorCode::Internal,
            format!("failed to parse llm response: {err}"),
        )
    })
}

fn io_error(op: &str, err: std::io::Error) -> CapabilityError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => CapabilityErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => CapabilityErrorCode::Denied,
        _ => CapabilityErrorCode::Internal,
    };
    capability_error(code, format!("{op} failed: {err}"))
}

fn path_error(err: sandbox::PathError) -> CapabilityError {
    let code = match err {
        sandbox::PathError::Escapes(_) | sandbox::PathError::Dangling(_) => {
            CapabilityErrorCode::Denied
        }
        sandbox::PathError::Io(_) => CapabilityErrorCode::Internal,
        _ => CapabilityErrorCode::InvalidArgument,
    };
    capability_error(code, err.to_string())
}

/// `relative` under the handle's directory `parent`, confined to the workspace ([`sandbox::confine`]).
fn confined(
    state: &HostState,
    parent: &Utf8Path,
    relative: &str,
) -> Result<Utf8PathBuf, CapabilityError> {
    sandbox::confine(&state.config.workspace_root, parent, relative).map_err(path_error)
}

/// Like [`confined`], without following the last component (for removing and renaming entries).
fn confined_entry(
    state: &HostState,
    parent: &Utf8Path,
    relative: &str,
) -> Result<Utf8PathBuf, CapabilityError> {
    sandbox::confine_entry(&state.config.workspace_root, parent, relative).map_err(path_error)
}

fn denied(why: impl Into<String>) -> CapabilityError {
    capability_error(CapabilityErrorCode::Denied, why)
}

/// A browser failure: a refusal by the host's rules is `denied`, one past a cap `limit`, anything
/// else `internal`. The browser says which ([`browser::Refusal`]); the message is not consulted.
fn browser_error(err: anyhow::Error) -> CapabilityError {
    let code = match err.downcast_ref::<browser::Refusal>() {
        Some(browser::Refusal::Denied(_)) => CapabilityErrorCode::Denied,
        Some(browser::Refusal::Limit(_)) => CapabilityErrorCode::Limit,
        None => CapabilityErrorCode::Internal,
    };
    capability_error(code, err.to_string())
}

/// A refusal because a budget or a cap is spent.
fn limit(why: impl Into<String>) -> CapabilityError {
    capability_error(CapabilityErrorCode::Limit, why)
}

impl HostState {
    /// Pay for one `kind` operation from the run's budgets.
    fn charge(&self, kind: Kind) -> Result<(), CapabilityError> {
        budget::claim(&self.budgets, kind, 1)
            .map(|_| ())
            .map_err(limit)
    }

    /// Wall-clock time left in the run; none left refuses the call.
    fn time_left(&self) -> Result<Duration, CapabilityError> {
        let left = self
            .budgets
            .lock()
            .map(|b| b.remaining_wallclock())
            .unwrap_or_default();
        if left.is_zero() {
            return Err(limit("the wall-clock budget is spent"));
        }
        Ok(left)
    }

    /// Count one event or grant request of `bytes`, within [`MAX_EVENT_BYTES`] and [`MAX_EVENTS`].
    fn note_event(&mut self, bytes: usize) -> Result<(), CapabilityError> {
        if bytes > MAX_EVENT_BYTES {
            return Err(capability_error(
                CapabilityErrorCode::InvalidArgument,
                format!("an event may hold at most {MAX_EVENT_BYTES} bytes ({bytes} given)"),
            ));
        }
        if self.events >= MAX_EVENTS {
            return Err(limit(format!(
                "the agent may log at most {MAX_EVENTS} events and grant requests in a run"
            )));
        }
        self.events += 1;
        Ok(())
    }

    /// Refuse a write when the workspace is read-only (`[workspace] read_only`).
    fn writable(&self) -> Result<(), CapabilityError> {
        if self.config.workspace_rules.read_only {
            Err(denied("the workspace is read-only"))
        } else {
            Ok(())
        }
    }

    /// Refuse a call to the LLM once the token budget is spent.
    fn tokens_left(&self) -> Result<(), CapabilityError> {
        self.budgets
            .lock()
            .map_err(|_| denied("the budgets are unavailable"))?
            .check(Kind::LlmTokens)
            .map_err(|why| limit(format!("the token budget ran out: {why}")))
    }

    /// Tokens left in the run's budget.
    fn tokens_remaining(&self) -> u64 {
        self.budgets
            .lock()
            .map(|b| b.remaining(Kind::LlmTokens))
            .unwrap_or_default()
    }

    /// Record the tokens a call used ([`tokens_charged`]).
    fn spend_tokens(&self, tokens: u64) {
        if let Ok(mut budgets) = self.budgets.lock() {
            budgets.spend(Kind::LlmTokens, tokens);
        }
    }

    fn browser(&self) -> Result<Browser<'_>, CapabilityError> {
        let settings = self
            .config
            .browser
            .as_ref()
            .ok_or_else(|| denied("browser capability is disabled in host configuration"))?;
        let remaining = self
            .budgets
            .lock()
            .map(|b| b.remaining_wallclock())
            .unwrap_or_default();
        Ok(Browser {
            tokio: &self.tokio,
            settings,
            workspace: &self.config.workspace_root,
            read_only: self.config.workspace_rules.read_only,
            sessions: &self.sessions,
            remaining,
        })
    }
}

fn dir_path<'a>(
    state: &'a HostState,
    handle: &Resource<DirHandle>,
) -> Result<&'a Utf8Path, CapabilityError> {
    state
        .resources
        .get(handle)
        .map(|dir: &DirHandleResource| dir.path.as_ref())
        .map_err(table_error)
}

fn dir_path_buf(
    state: &HostState,
    handle: &Resource<DirHandle>,
) -> Result<Utf8PathBuf, CapabilityError> {
    dir_path(state, handle).map(|p| p.to_path_buf())
}

fn insert_dir(
    state: &mut HostState,
    path: Utf8PathBuf,
) -> Result<Resource<DirHandle>, CapabilityError> {
    state
        .resources
        .push(DirHandleResource { path })
        .map_err(table_error)
}

fn insert_file(
    state: &mut HostState,
    entry: FileHandleResource,
) -> Result<Resource<FileHandle>, CapabilityError> {
    state.resources.push(entry).map_err(table_error)
}

fn file_entry_mut<'a>(
    state: &'a mut HostState,
    handle: &Resource<FileHandle>,
) -> Result<&'a mut FileHandleResource, CapabilityError> {
    state.resources.get_mut(handle).map_err(table_error)
}

fn delete_dir(state: &mut HostState, handle: Resource<DirHandle>) -> Result<(), CapabilityError> {
    let _ = state.resources.delete(handle).map_err(table_error)?;
    Ok(())
}

fn delete_file(state: &mut HostState, handle: Resource<FileHandle>) -> Result<(), CapabilityError> {
    let _ = state.resources.delete(handle).map_err(table_error)?;
    Ok(())
}

fn process_entry_mut<'a>(
    state: &'a mut HostState,
    handle: &Resource<ProcHandle>,
) -> Result<&'a mut ProcessResource, CapabilityError> {
    state.resources.get_mut(handle).map_err(table_error)
}

fn delete_process(
    state: &mut HostState,
    handle: Resource<ProcHandle>,
) -> Result<(), CapabilityError> {
    let _ = state.resources.delete(handle).map_err(table_error)?;
    Ok(())
}

fn metadata_to_entry(
    entry_name: String,
    meta: fs::Metadata,
) -> bindings::osagent::fs::fs::DirEntry {
    bindings::osagent::fs::fs::DirEntry {
        name: entry_name,
        kind: entry_kind(&meta),
        size_bytes: Some(meta.len()),
        modified_ms: file_time_ms(&meta),
    }
}

fn entry_kind(meta: &fs::Metadata) -> bindings::osagent::fs::fs::EntryKind {
    if meta.is_file() {
        bindings::osagent::fs::fs::EntryKind::File
    } else if meta.is_dir() {
        bindings::osagent::fs::fs::EntryKind::Directory
    } else if meta.file_type().is_symlink() {
        bindings::osagent::fs::fs::EntryKind::Symlink
    } else {
        bindings::osagent::fs::fs::EntryKind::Other
    }
}

fn file_time_ms(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64)
}

fn read_file_bytes(
    state: &mut HostState,
    handle: &Resource<FileHandle>,
    max_bytes: u64,
    op: &str,
) -> Result<Vec<u8>, CapabilityError> {
    let entry = file_entry_mut(state, handle)?;
    let mut reader = (&mut entry.file).take(sandbox::cap_read(max_bytes));
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(|err| io_error(op, err))?;
    Ok(buf)
}

fn write_file_bytes(
    state: &mut HostState,
    handle: &Resource<FileHandle>,
    data: &[u8],
    op: &str,
) -> Result<u64, CapabilityError> {
    state.writable()?;
    let max = state.config.workspace_rules.max_file_bytes;
    let entry = file_entry_mut(state, handle)?;
    // Wherever the write lands (the cursor, or the end when appending), the file ends no later than
    // this.
    let pos = entry
        .file
        .stream_position()
        .map_err(|err| io_error(op, err))?;
    let len = entry
        .file
        .metadata()
        .map(|m| m.len())
        .map_err(|err| io_error(op, err))?;
    if pos.max(len).saturating_add(data.len() as u64) > max {
        return Err(denied(format!(
            "the write would make the file larger than max_file_bytes ({max})"
        )));
    }
    entry
        .file
        .write(data)
        .map(|written| written as u64)
        .map_err(|err| io_error(op, err))
}

fn to_exit_status(resource: &ProcessResource) -> bindings::osagent::proc::proc::ExitStatus {
    bindings::osagent::proc::proc::ExitStatus {
        code: resource.exit_code,
        signal: None,
        timed_out: resource.timed_out,
    }
}

fn read_process_stream(
    data: &[u8],
    offset: &mut usize,
    max_bytes: u32,
) -> bindings::osagent::proc::proc::StreamRead {
    let max = max_bytes as usize;
    let remaining = data.len().saturating_sub(*offset);
    let take = remaining.min(max);
    let start = *offset;
    let end = start + take;
    let chunk = data[start..end].to_vec();
    *offset = end;
    bindings::osagent::proc::proc::StreamRead {
        data: chunk,
        eof: *offset >= data.len(),
    }
}

impl bindings::osagent::common::types::Host for HostState {}

impl bindings::osagent::fs::fs::Host for HostState {
    fn open_workspace(&mut self) -> Result<Resource<DirHandle>, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        insert_dir(self, self.config.workspace_root.clone())
    }

    fn open_dir(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
    ) -> Result<Resource<DirHandle>, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let candidate = confined(self, &parent_path, &relative_path)?;
        let metadata =
            fs::metadata(candidate.as_std_path()).map_err(|err| io_error("fs.open-dir", err))?;
        if !metadata.is_dir() {
            return Err(capability_error(
                CapabilityErrorCode::InvalidArgument,
                "path is not a directory",
            ));
        }
        insert_dir(self, candidate)
    }

    fn ensure_dir(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
    ) -> Result<Resource<DirHandle>, CapabilityError> {
        self.writable()?;
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let candidate = confined(self, &parent_path, &relative_path)?;
        fs::create_dir_all(candidate.as_std_path())
            .map_err(|err| io_error("fs.ensure-dir", err))?;
        insert_dir(self, candidate)
    }

    fn remove_dir(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
        recursive: bool,
    ) -> Result<(), CapabilityError> {
        self.writable()?;
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let target = confined_entry(self, &parent_path, &relative_path)?;
        if recursive {
            fs::remove_dir_all(target.as_std_path()).map_err(|err| io_error("fs.remove-dir", err))
        } else {
            fs::remove_dir(target.as_std_path()).map_err(|err| io_error("fs.remove-dir", err))
        }
    }

    fn remove_file(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
    ) -> Result<(), CapabilityError> {
        self.writable()?;
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let target = confined_entry(self, &parent_path, &relative_path)?;
        fs::remove_file(target.as_std_path()).map_err(|err| io_error("fs.remove-file", err))
    }

    fn rename(
        &mut self,
        parent: Resource<DirHandle>,
        old_path: String,
        new_path: String,
    ) -> Result<(), CapabilityError> {
        self.writable()?;
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let from = confined_entry(self, &parent_path, &old_path)?;
        let to = confined_entry(self, &parent_path, &new_path)?;
        fs::rename(from.as_std_path(), to.as_std_path()).map_err(|err| io_error("fs.rename", err))
    }

    fn list_dir(
        &mut self,
        target: Resource<DirHandle>,
    ) -> Result<Vec<bindings::osagent::fs::fs::DirEntry>, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        // Re-checked on every use: the handle's entry may have been swapped for a symlink since it
        // was opened.
        let dir_path = dir_path(self, &target)?.to_path_buf();
        let dir_path = confined(self, &dir_path, "")?;
        let mut entries = Vec::new();
        let read = fs::read_dir(&dir_path).map_err(|err| io_error("fs.list-dir", err))?;
        for entry in read {
            let entry = entry.map_err(|err| io_error("fs.list-dir", err))?;
            let name = entry
                .file_name()
                .into_string()
                .unwrap_or_else(|os| os.to_string_lossy().into_owned());
            let metadata = entry
                .metadata()
                .map_err(|err| io_error("fs.list-dir", err))?;
            entries.push(metadata_to_entry(name, metadata));
        }
        Ok(entries)
    }

    fn metadata(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: Option<String>,
    ) -> Result<bindings::osagent::fs::fs::EntryMetadata, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        let base = dir_path(self, &parent)?.to_path_buf();
        // With no relative path, the handle's own path, re-checked (it may have been swapped).
        let path = confined(self, &base, relative_path.as_deref().unwrap_or(""))?;
        let metadata =
            fs::metadata(path.as_std_path()).map_err(|err| io_error("fs.metadata", err))?;
        Ok(bindings::osagent::fs::fs::EntryMetadata {
            name: path
                .file_name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| String::from(".")),
            kind: entry_kind(&metadata),
            size_bytes: Some(metadata.len()),
            modified_ms: file_time_ms(&metadata),
            readonly: metadata.permissions().readonly(),
        })
    }

    fn open_file(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
        options: bindings::osagent::fs::fs::FileOpenOptions,
    ) -> Result<Resource<FileHandle>, CapabilityError> {
        if options.write || options.append || options.create || options.truncate {
            self.writable()?;
        }
        self.charge(Kind::FilesystemOps)?;
        let parent_path = dir_path_buf(self, &parent)?;
        let file_path = confined(self, &parent_path, &relative_path)?;
        let mut open_opts = OpenOptions::new();
        open_opts.read(options.read);
        open_opts.write(options.write || options.append);
        open_opts.append(options.append);
        open_opts.create(options.create);
        open_opts.truncate(options.truncate);
        let file = open_opts
            .open(file_path.as_std_path())
            .map_err(|err| io_error("fs.open-file", err))?;
        insert_file(
            self,
            FileHandleResource {
                path: file_path,
                file,
            },
        )
    }
}

impl bindings::osagent::fs::fs::HostDirHandle for HostState {
    /// Nothing to release for a directory; the entry goes when the guest drops the handle. (`close`
    /// gets a borrowed handle, which the table must not delete.)
    fn close(&mut self, _handle: Resource<DirHandle>) {}

    fn drop(&mut self, handle: Resource<DirHandle>) -> wasmtime::Result<()> {
        let _ = delete_dir(self, handle);
        Ok(())
    }
}

impl bindings::osagent::fs::fs::HostFileHandle for HostState {
    fn read(
        &mut self,
        handle: Resource<FileHandle>,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        read_file_bytes(self, &handle, max_bytes, "fs.file.read")
    }

    fn read_to_string(
        &mut self,
        handle: Resource<FileHandle>,
        max_bytes: u64,
    ) -> Result<String, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        let bytes = read_file_bytes(self, &handle, max_bytes, "fs.file.read-to-string")?;
        String::from_utf8(bytes).map_err(|_| {
            capability_error(
                CapabilityErrorCode::InvalidArgument,
                "file is not valid UTF-8",
            )
        })
    }

    fn write(
        &mut self,
        handle: Resource<FileHandle>,
        bytes: Vec<u8>,
    ) -> Result<u64, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        write_file_bytes(self, &handle, &bytes, "fs.file.write")
    }

    fn write_string(
        &mut self,
        handle: Resource<FileHandle>,
        contents: String,
        newline: bool,
    ) -> Result<u64, CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        let mut data = contents.into_bytes();
        if newline {
            data.push(b'\n');
        }
        write_file_bytes(self, &handle, &data, "fs.file.write-string")
    }

    fn set_len(
        &mut self,
        handle: Resource<FileHandle>,
        new_len: u64,
    ) -> Result<(), CapabilityError> {
        self.writable()?;
        self.charge(Kind::FilesystemOps)?;
        let max = self.config.workspace_rules.max_file_bytes;
        if new_len > max {
            return Err(denied(format!(
                "the file would be larger than max_file_bytes ({max})"
            )));
        }
        let file = file_entry_mut(self, &handle)?;
        file.file
            .set_len(new_len)
            .map_err(|err| io_error("fs.file.set-len", err))
    }

    fn flush(&mut self, handle: Resource<FileHandle>) -> Result<(), CapabilityError> {
        self.charge(Kind::FilesystemOps)?;
        let file = file_entry_mut(self, &handle)?;
        file.file
            .flush()
            .map_err(|err| io_error("fs.file.flush", err))
    }

    /// Flushes the file; the entry (and the open file) goes when the guest drops the handle. (`close`
    /// gets a borrowed handle, which the table must not delete.)
    fn close(&mut self, handle: Resource<FileHandle>) {
        if let Ok(file) = file_entry_mut(self, &handle) {
            let _ = file.file.flush();
        }
    }

    fn drop(&mut self, handle: Resource<FileHandle>) -> wasmtime::Result<()> {
        let _ = delete_file(self, handle);
        Ok(())
    }
}

impl bindings::osagent::proc::proc::Host for HostState {
    fn spawn(
        &mut self,
        command: String,
        options: bindings::osagent::proc::proc::SpawnOptions,
    ) -> Result<Resource<ProcHandle>, CapabilityError> {
        self.charge(Kind::Processes)?;
        let Some(program) = self.config.program_for(&command).map(Path::to_path_buf) else {
            return Err(capability_error(
                CapabilityErrorCode::Denied,
                format!("command `{command}` is not allowed"),
            ));
        };
        for env in &options.env {
            sandbox::check_env_key(&env.key, &self.config.allowed_env)
                .map_err(|why| capability_error(CapabilityErrorCode::Denied, why))?;
        }

        if !matches!(
            options.stdin,
            bindings::osagent::proc::proc::StdioMode::Null
        ) {
            return Err(capability_error(
                CapabilityErrorCode::InvalidArgument,
                "stdin must be null for now",
            ));
        }
        if !matches!(
            options.stdout,
            bindings::osagent::proc::proc::StdioMode::Pipe
        ) || !matches!(
            options.stderr,
            bindings::osagent::proc::proc::StdioMode::Pipe
        ) {
            return Err(capability_error(
                CapabilityErrorCode::InvalidArgument,
                "stdout/stderr must be pipe",
            ));
        }

        // The allowlist's resolved program, never a PATH lookup of the guest's string.
        let mut cmd = Command::new(program);
        for arg in options.argv {
            cmd.arg(arg);
        }

        let working_dir = if let Some(dir) = options.working_dir {
            let root = self.config.workspace_root.clone();
            let resolved = confined(self, &root, &dir)?;
            Some(resolved)
        } else {
            None
        };
        if let Some(dir) = working_dir.as_ref() {
            cmd.current_dir(dir.as_std_path());
        } else {
            cmd.current_dir(self.config.workspace_root.as_std_path());
        }

        cmd.env_clear();
        // The host's PATH, outside the workspace; the guest cannot set it.
        cmd.env("PATH", &self.config.child_path);
        for env in options.env {
            // A secret reference becomes its value here, for the process only.
            let value = self
                .config
                .secrets
                .resolve(&env.value)
                .map_err(|why| denied(why.to_string()))?;
            cmd.env(env.key, value);
        }

        // Runs to completion here (bounded in time and output); the handle then serves its results.
        // Never past the run's wall-clock budget.
        let remaining = self.time_left()?;
        let timeout = sandbox::proc_timeout(options.timeout_ms).min(remaining);
        let output =
            sandbox::run_bounded(cmd, timeout).map_err(|err| io_error("proc.spawn", err))?;
        let secrets = &self.config.secrets;
        let resource = ProcessResource {
            command: command.clone(),
            stdout: secrets.redact_bytes(&output.stdout),
            stderr: secrets.redact_bytes(&output.stderr),
            stdout_pos: 0,
            stderr_pos: 0,
            exit_code: output.status,
            timed_out: output.timed_out,
        };
        insert_process(self, resource)
    }
}

fn insert_process(
    state: &mut HostState,
    proc: ProcessResource,
) -> Result<Resource<ProcHandle>, CapabilityError> {
    state.resources.push(proc).map_err(table_error)
}

impl bindings::osagent::proc::proc::HostProcess for HostState {
    fn write_stdin(
        &mut self,
        _: Resource<ProcHandle>,
        _chunk: Vec<u8>,
        _eof: bool,
    ) -> Result<u32, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "stdin is closed: proc.spawn runs a process to completion with stdin null",
        ))
    }

    fn read_stdout(
        &mut self,
        handle: Resource<ProcHandle>,
        max_bytes: u32,
    ) -> Result<bindings::osagent::proc::proc::StreamRead, CapabilityError> {
        let process = process_entry_mut(self, &handle)?;
        Ok(read_process_stream(
            &process.stdout,
            &mut process.stdout_pos,
            max_bytes,
        ))
    }

    fn read_stderr(
        &mut self,
        handle: Resource<ProcHandle>,
        max_bytes: u32,
    ) -> Result<bindings::osagent::proc::proc::StreamRead, CapabilityError> {
        let process = process_entry_mut(self, &handle)?;
        Ok(read_process_stream(
            &process.stderr,
            &mut process.stderr_pos,
            max_bytes,
        ))
    }

    fn wait(
        &mut self,
        handle: Resource<ProcHandle>,
        _timeout_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<bindings::osagent::proc::proc::ExitStatus, CapabilityError> {
        let process = process_entry_mut(self, &handle)?;
        Ok(to_exit_status(process))
    }

    fn signal(
        &mut self,
        _rep: Resource<ProcHandle>,
        _kind: bindings::osagent::proc::proc::ProcessSignal,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "signaling processes is not supported",
        ))
    }

    /// Releases the buffered output; the entry goes when the guest drops the handle. (`close` gets a
    /// borrowed handle, which the table must not delete.)
    fn close(&mut self, handle: Resource<ProcHandle>) {
        if let Ok(process) = process_entry_mut(self, &handle) {
            process.stdout = Vec::new();
            process.stderr = Vec::new();
            process.stdout_pos = 0;
            process.stderr_pos = 0;
        }
    }

    fn drop(&mut self, handle: Resource<ProcHandle>) -> wasmtime::Result<()> {
        let _ = delete_process(self, handle);
        Ok(())
    }
}

type Session = bindings::osagent::browser::browser::Session;
type ElementHandle = bindings::osagent::browser::browser::ElementHandle;

fn select(selector: bindings::osagent::browser::browser::Selector) -> Select {
    use bindings::osagent::browser::browser::SelectorKind;
    match selector.kind {
        SelectorKind::Css => Select::Css(selector.value),
        SelectorKind::Xpath => Select::XPath(selector.value),
        SelectorKind::Text => Select::Text(selector.value),
    }
}

fn wait_for(timeout_ms: Option<u64>) -> std::time::Duration {
    Duration::from_millis(timeout_ms.unwrap_or(5_000))
}

impl HostState {
    fn session_driver(
        &self,
        session: &Resource<Session>,
    ) -> Result<thirtyfour::WebDriver, CapabilityError> {
        self.resources
            .get(session)
            .map(|s: &BrowserSessionResource| s.driver.clone())
            .map_err(table_error)
    }

    fn element_parts(
        &self,
        element: &Resource<ElementHandle>,
    ) -> Result<(thirtyfour::WebDriver, thirtyfour::WebElement), CapabilityError> {
        self.resources
            .get(element)
            .map(|e: &BrowserElementResource| (e.driver.clone(), e.element.clone()))
            .map_err(table_error)
    }

    fn insert_element(
        &mut self,
        driver: thirtyfour::WebDriver,
        element: thirtyfour::WebElement,
    ) -> Result<Resource<ElementHandle>, CapabilityError> {
        self.resources
            .push(BrowserElementResource { element, driver })
            .map_err(table_error)
    }
}

impl bindings::osagent::browser::browser::Host for HostState {
    fn open_session(
        &mut self,
        options: bindings::osagent::browser::browser::SessionOptions,
    ) -> Result<Resource<Session>, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self
            .browser()?
            .open(options.headless, options.allow_downloads)
            .map_err(browser_error)?;
        self.resources
            .push(BrowserSessionResource { driver })
            .map_err(table_error)
    }
}

impl bindings::osagent::browser::browser::HostSession for HostState {
    /// Ends the browser session; the entry goes when the guest drops the handle.
    fn close(&mut self, session: Resource<Session>) {
        if let (Ok(driver), Ok(browser)) = (self.session_driver(&session), self.browser()) {
            browser.quit(driver);
        }
    }

    fn drop(&mut self, session: Resource<Session>) -> wasmtime::Result<()> {
        if let Ok(entry) = self.resources.delete(session)
            && let Ok(browser) = self.browser()
        {
            browser.quit(entry.driver);
        }
        Ok(())
    }

    fn goto(
        &mut self,
        session: Resource<Session>,
        url: String,
        timeout_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<bindings::osagent::browser::browser::PageState, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self.session_driver(&session)?;
        let landed = self
            .browser()?
            .goto(&driver, &url, wait_for(timeout_ms))
            .map_err(browser_error)?;
        Ok(bindings::osagent::browser::browser::PageState {
            url: landed,
            title: None,
            html: None,
        })
    }

    fn describe_page(
        &mut self,
        session: Resource<Session>,
        include_html: bool,
    ) -> Result<bindings::osagent::browser::browser::PageState, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self.session_driver(&session)?;
        let page = self
            .browser()?
            .describe(&driver, include_html)
            .map_err(browser_error)?;
        let secrets = &self.config.secrets;
        Ok(bindings::osagent::browser::browser::PageState {
            url: secrets.redact(&page.url),
            title: page.title.map(|t| secrets.redact(&t)),
            html: page.html.map(|h| secrets.redact(&h)),
        })
    }

    /// Always a PNG (the WebDriver protocol offers no other format); `mime-type` says so.
    fn screenshot(
        &mut self,
        session: Resource<Session>,
        _kind: bindings::osagent::browser::browser::ScreenshotKind,
    ) -> Result<bindings::osagent::browser::browser::Screenshot, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self.session_driver(&session)?;
        let data = self.browser()?.screenshot(&driver).map_err(browser_error)?;
        Ok(bindings::osagent::browser::browser::Screenshot {
            mime_type: "image/png".to_string(),
            data,
        })
    }

    /// Refused: a script could reach any host from the page, around `allowed_hosts`.
    fn eval(
        &mut self,
        _session: Resource<Session>,
        _expression: String,
    ) -> Result<bindings::osagent::common::types::Json, CapabilityError> {
        Err(denied(
            "script evaluation is disabled: a script could reach any host and bypass allowed_hosts",
        ))
    }

    fn find(
        &mut self,
        session: Resource<Session>,
        selector: bindings::osagent::browser::browser::Selector,
        timeout_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<Resource<ElementHandle>, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self.session_driver(&session)?;
        let element = self
            .browser()?
            .find(&driver, &select(selector), wait_for(timeout_ms))
            .map_err(browser_error)?;
        self.insert_element(driver, element)
    }

    fn query_all(
        &mut self,
        session: Resource<Session>,
        selector: bindings::osagent::browser::browser::Selector,
    ) -> Result<Vec<Resource<ElementHandle>>, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let driver = self.session_driver(&session)?;
        let elements = self
            .browser()?
            .find_all(&driver, &select(selector))
            .map_err(browser_error)?;
        elements
            .into_iter()
            .map(|element| self.insert_element(driver.clone(), element))
            .collect()
    }
}

impl bindings::osagent::browser::browser::HostElementHandle for HostState {
    fn click(&mut self, element: Resource<ElementHandle>) -> Result<(), CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        self.browser()?
            .click(&driver, &element)
            .map_err(browser_error)
    }

    fn type_text(
        &mut self,
        element: Resource<ElementHandle>,
        text: String,
        submit: bool,
    ) -> Result<(), CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        // A secret reference becomes its value here, on its way into the page, and nowhere else.
        let text = self
            .config
            .secrets
            .resolve(&text)
            .map_err(|why| denied(why.to_string()))?;
        self.browser()?
            .type_text(&driver, &element, &text, submit)
            .map_err(browser_error)
    }

    fn clear(&mut self, element: Resource<ElementHandle>) -> Result<(), CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        self.browser()?
            .clear(&driver, &element)
            .map_err(browser_error)
    }

    fn attribute(
        &mut self,
        element: Resource<ElementHandle>,
        name: String,
    ) -> Result<Option<String>, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        let value = self
            .browser()?
            .attribute(&driver, &element, &name)
            .map_err(browser_error)?;
        Ok(value.map(|v| self.config.secrets.redact(&v)))
    }

    fn inner_text(&mut self, element: Resource<ElementHandle>) -> Result<String, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        let text = self
            .browser()?
            .inner_text(&driver, &element)
            .map_err(browser_error)?;
        Ok(self.config.secrets.redact(&text))
    }

    fn html(&mut self, element: Resource<ElementHandle>) -> Result<String, CapabilityError> {
        self.charge(Kind::BrowserActions)?;
        let (driver, element) = self.element_parts(&element)?;
        let html = self
            .browser()?
            .outer_html(&driver, &element)
            .map_err(browser_error)?;
        Ok(self.config.secrets.redact(&html))
    }

    fn drop(&mut self, element: Resource<ElementHandle>) -> wasmtime::Result<()> {
        let _ = self.resources.delete(element);
        Ok(())
    }
}

impl bindings::osagent::input::input::Host for HostState {
    fn key_sequence(&mut self, _text: String) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input is disabled: wasi-warden ships no keyboard or mouse backend",
        ))
    }

    fn send_key_chord(
        &mut self,
        _chord: bindings::osagent::input::input::KeyChord,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input is disabled: wasi-warden ships no keyboard or mouse backend",
        ))
    }

    fn mouse_move(
        &mut self,
        _motion: bindings::osagent::input::input::PointerMove,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input is disabled: wasi-warden ships no keyboard or mouse backend",
        ))
    }

    fn mouse_click(
        &mut self,
        _button: bindings::osagent::input::input::MouseButton,
        _hold_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input is disabled: wasi-warden ships no keyboard or mouse backend",
        ))
    }

    fn mouse_scroll(
        &mut self,
        _delta: bindings::osagent::input::input::ScrollDelta,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input is disabled: wasi-warden ships no keyboard or mouse backend",
        ))
    }
}

impl bindings::osagent::llm::llm::Host for HostState {
    fn complete(
        &mut self,
        messages: Vec<bindings::osagent::llm::llm::Message>,
        options: bindings::osagent::llm::llm::Options,
    ) -> Result<bindings::osagent::llm::llm::CompletionResponse, CapabilityError> {
        self.tokens_left()?;
        let timeout = self.time_left()?.min(LLM_TIMEOUT);
        let settings = require_llm_settings(&self.config)?;
        let mut options = options;
        options.max_tokens = clamp_max_tokens(options.max_tokens, self.tokens_remaining());
        let request = build_chat_request(
            &settings.model,
            messages,
            options,
            None,
            &self.config.secrets,
        );
        let response = execute_chat_request(settings, &request, timeout)?;
        let reply_bytes = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .map_or(0, String::len);
        self.spend_tokens(tokens_charged(
            response.usage.as_ref(),
            &request,
            reply_bytes,
        ));
        let usage = convert_usage(response.usage);
        let mut choices = response.choices.into_iter();
        let choice = choices.next().ok_or_else(|| {
            capability_error(
                CapabilityErrorCode::Internal,
                "llm response missing choices",
            )
        })?;
        let content = choice.message.content.ok_or_else(|| {
            capability_error(
                CapabilityErrorCode::Internal,
                "llm response missing content",
            )
        })?;
        Ok(bindings::osagent::llm::llm::CompletionResponse {
            content,
            finish_reason: choice.finish_reason,
            usage,
        })
    }

    fn call_tools(
        &mut self,
        messages: Vec<bindings::osagent::llm::llm::Message>,
        tools: Vec<bindings::osagent::llm::llm::ToolSchema>,
        options: bindings::osagent::llm::llm::Options,
    ) -> Result<bindings::osagent::llm::llm::ToolResponse, CapabilityError> {
        self.tokens_left()?;
        let timeout = self.time_left()?.min(LLM_TIMEOUT);
        let settings = require_llm_settings(&self.config)?;
        let chat_tools = tools_to_chat(tools, &self.config.secrets)?;
        let mut options = options;
        options.max_tokens = clamp_max_tokens(options.max_tokens, self.tokens_remaining());
        let request = build_chat_request(
            &settings.model,
            messages,
            options,
            Some(chat_tools),
            &self.config.secrets,
        );
        let response = execute_chat_request(settings, &request, timeout)?;
        let reply_bytes = response.choices.first().map_or(0, |c| {
            serde_json::to_vec(&c.message.tool_calls).map_or(0, |v| v.len())
        });
        self.spend_tokens(tokens_charged(
            response.usage.as_ref(),
            &request,
            reply_bytes,
        ));
        let usage = convert_usage(response.usage);
        let mut choices = response.choices.into_iter();
        let choice = choices.next().ok_or_else(|| {
            capability_error(
                CapabilityErrorCode::Internal,
                "llm response missing choices",
            )
        })?;
        if choice.message.tool_calls.is_empty() {
            return Err(capability_error(
                CapabilityErrorCode::Internal,
                "llm response missing tool calls",
            ));
        }
        let tool_calls_json = serde_json::to_string(&choice.message.tool_calls).map_err(|err| {
            capability_error(
                CapabilityErrorCode::Internal,
                format!("failed to encode tool calls: {err}"),
            )
        })?;
        Ok(bindings::osagent::llm::llm::ToolResponse {
            tool_calls_json,
            finish_reason: choice.finish_reason,
            usage,
        })
    }
}

fn kind_of(kind: bindings::osagent::policy::policy::BudgetKind) -> Kind {
    use bindings::osagent::policy::policy::BudgetKind;
    match kind {
        BudgetKind::Steps => Kind::Steps,
        BudgetKind::WallclockMs => Kind::WallclockMs,
        BudgetKind::LlmTokens => Kind::LlmTokens,
        BudgetKind::Processes => Kind::Processes,
        BudgetKind::FilesystemOps => Kind::FilesystemOps,
        BudgetKind::BrowserActions => Kind::BrowserActions,
    }
}

fn wit_kind(kind: Kind) -> bindings::osagent::policy::policy::BudgetKind {
    use bindings::osagent::policy::policy::BudgetKind;
    match kind {
        Kind::Steps => BudgetKind::Steps,
        Kind::WallclockMs => BudgetKind::WallclockMs,
        Kind::LlmTokens => BudgetKind::LlmTokens,
        Kind::Processes => BudgetKind::Processes,
        Kind::FilesystemOps => BudgetKind::FilesystemOps,
        Kind::BrowserActions => BudgetKind::BrowserActions,
    }
}

impl HostState {
    fn budget_snapshot(&self, kind: Kind) -> bindings::osagent::policy::policy::BudgetSnapshot {
        let (limit, used) = self
            .budgets
            .lock()
            .map(|b| (b.limits().of(kind), b.used(kind)))
            .unwrap_or_default();
        bindings::osagent::policy::policy::BudgetSnapshot {
            kind: wit_kind(kind),
            limit: Some(limit),
            used,
        }
    }
}

/// The policy as hostd.toml set it. It is fixed for the run: the guest can read it, spend its
/// budgets and log events, but no grant widens it.
impl bindings::osagent::policy::policy::Host for HostState {
    fn describe(
        &mut self,
    ) -> Result<bindings::osagent::policy::policy::PolicySnapshot, CapabilityError> {
        use bindings::osagent::policy::policy as wit;
        let rules = self.config.workspace_rules;
        Ok(wit::PolicySnapshot {
            workspaces: vec![wit::WorkspaceRule {
                label: "workspace".to_string(),
                relative_root: ".".to_string(),
                read_only: rules.read_only,
                max_file_bytes: Some(rules.max_file_bytes),
            }],
            commands: self
                .config
                .allowed_proc_commands
                .iter()
                .map(|name| wit::CommandRule {
                    name: name.clone(),
                    description: None,
                })
                .collect(),
            browser: self.config.browser.as_ref().map(|b| wit::BrowserRule {
                allowed_hosts: b.allowed_hosts.clone(),
                allow_screenshots: true,
                allow_file_uploads: false,
            }),
            budgets: Kind::ALL.iter().map(|k| self.budget_snapshot(*k)).collect(),
        })
    }

    fn claim_budget(
        &mut self,
        kind: bindings::osagent::policy::policy::BudgetKind,
        units: u64,
    ) -> Result<bindings::osagent::policy::policy::BudgetSnapshot, CapabilityError> {
        let kind = kind_of(kind);
        budget::claim(&self.budgets, kind, units).map_err(limit)?;
        Ok(self.budget_snapshot(kind))
    }

    fn request_capability(
        &mut self,
        request: bindings::osagent::policy::policy::GrantRequest,
    ) -> Result<bindings::osagent::policy::policy::GrantResponse, CapabilityError> {
        self.note_event(
            request.capability.len()
                + request.scope.as_ref().map_or(0, String::len)
                + request.justification.len(),
        )?;
        tracing::info!(
            target: "audit",
            capability = request.capability,
            scope = request.scope,
            "grant requested and refused: the policy is fixed by hostd.toml"
        );
        Ok(bindings::osagent::policy::policy::GrantResponse {
            approved: false,
            reason: Some("no grants are configured: the policy is fixed by hostd.toml".to_string()),
        })
    }

    fn log_event(
        &mut self,
        event: bindings::osagent::common::types::AuditEvent,
    ) -> Result<(), CapabilityError> {
        self.note_event(event.event_type.len() + event.payload.len())?;
        tracing::info!(
            target: "audit",
            event_type = event.event_type,
            step = event.step,
            payload = event.payload,
            "agent event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(content: &str) -> ChatRequest {
        let options = bindings::osagent::llm::llm::Options {
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            presence_penalty: None,
            frequency_penalty: None,
        };
        let message = bindings::osagent::llm::llm::Message {
            role: MessageRole::User,
            content: content.to_string(),
            name: None,
        };
        build_chat_request("m", vec![message], options, None, &Secrets::default())
    }

    fn usage(prompt: Option<u32>, completion: Option<u32>, total: Option<u32>) -> ChatUsage {
        ChatUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
        }
    }

    /// The endpoint's total is charged; without one, its prompt and completion counts; without
    /// either, an estimate (a token per four bytes), so a silent endpoint still spends the budget.
    #[test]
    fn a_call_is_charged_what_the_endpoint_reports_or_an_estimate() {
        let req = request(&"x".repeat(4000));
        let reported = usage(Some(10), Some(5), Some(42));
        assert_eq!(tokens_charged(Some(&reported), &req, 0), 42);
        let no_total = usage(Some(10), Some(5), None);
        assert_eq!(tokens_charged(Some(&no_total), &req, 0), 15);
        let estimate = tokens_charged(None, &req, 400);
        assert!(estimate >= 1100, "{estimate}");
        assert_eq!(
            tokens_charged(Some(&usage(None, None, None)), &req, 400),
            estimate,
            "an empty report is no report"
        );
    }

    #[test]
    fn max_tokens_is_clamped_to_the_budget_left() {
        assert_eq!(clamp_max_tokens(Some(4096), 100), Some(100));
        assert_eq!(clamp_max_tokens(Some(50), 100), Some(50));
        assert_eq!(clamp_max_tokens(None, 100), None);
        assert_eq!(clamp_max_tokens(Some(7), u64::MAX), Some(7));
    }
}
