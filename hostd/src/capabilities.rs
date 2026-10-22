use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::Agent;
use wasmtime::component::{Resource, ResourceTableError};

use crate::bindings;
use crate::config::{HostConfig, LlmSettings};
use crate::resources::{DirHandleResource, FileHandleResource, ProcessResource};
use crate::sandbox;
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

fn http_agent() -> Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .build()
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
    usage.map(|u| bindings::osagent::llm::llm::TokenUsage {
        prompt_tokens: u.prompt_tokens.unwrap_or_default(),
        completion_tokens: u.completion_tokens.unwrap_or_default(),
        total_tokens: u.total_tokens.unwrap_or_default(),
    })
}

fn messages_to_chat(messages: Vec<bindings::osagent::llm::llm::Message>) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .map(|msg| ChatMessage {
            role: match msg.role {
                MessageRole::System => "system".to_string(),
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
                MessageRole::Tool => "tool".to_string(),
            },
            content: msg.content,
            name: msg.name.filter(|n| !n.trim().is_empty()),
        })
        .collect()
}

fn tools_to_chat(
    tools: Vec<bindings::osagent::llm::llm::ToolSchema>,
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
                name: tool.name,
                description: Some(tool.description),
                parameters,
            },
        });
    }
    Ok(result)
}

fn build_chat_request(
    model: &str,
    messages: Vec<bindings::osagent::llm::llm::Message>,
    options: bindings::osagent::llm::llm::Options,
    tools: Option<Vec<ChatTool>>,
) -> ChatRequest {
    let stop = if options.stop.is_empty() {
        None
    } else {
        Some(options.stop)
    };
    ChatRequest {
        model: model.to_string(),
        messages: messages_to_chat(messages),
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
) -> Result<ChatResponse, CapabilityError> {
    let url = chat_endpoint(&settings.api_base);
    let agent = http_agent();
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
    let entry = file_entry_mut(state, handle)?;
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
        insert_dir(self, self.config.workspace_root.clone())
    }

    fn open_dir(
        &mut self,
        parent: Resource<DirHandle>,
        relative_path: String,
    ) -> Result<Resource<DirHandle>, CapabilityError> {
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
        let parent_path = dir_path_buf(self, &parent)?;
        let from = confined_entry(self, &parent_path, &old_path)?;
        let to = confined_entry(self, &parent_path, &new_path)?;
        fs::rename(from.as_std_path(), to.as_std_path()).map_err(|err| io_error("fs.rename", err))
    }

    fn list_dir(
        &mut self,
        target: Resource<DirHandle>,
    ) -> Result<Vec<bindings::osagent::fs::fs::DirEntry>, CapabilityError> {
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
        read_file_bytes(self, &handle, max_bytes, "fs.file.read")
    }

    fn read_to_string(
        &mut self,
        handle: Resource<FileHandle>,
        max_bytes: u64,
    ) -> Result<String, CapabilityError> {
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
        write_file_bytes(self, &handle, &bytes, "fs.file.write")
    }

    fn write_string(
        &mut self,
        handle: Resource<FileHandle>,
        contents: String,
        newline: bool,
    ) -> Result<u64, CapabilityError> {
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
        let file = file_entry_mut(self, &handle)?;
        file.file
            .set_len(new_len)
            .map_err(|err| io_error("fs.file.set-len", err))
    }

    fn flush(&mut self, handle: Resource<FileHandle>) -> Result<(), CapabilityError> {
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
            cmd.env(env.key, env.value);
        }

        // Runs to completion here (bounded in time and output); the handle then serves its results.
        let output = sandbox::run_bounded(cmd, sandbox::proc_timeout(options.timeout_ms))
            .map_err(|err| io_error("proc.spawn", err))?;
        let resource = ProcessResource {
            command: command.clone(),
            stdout: output.stdout,
            stderr: output.stderr,
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
            "process capability is not implemented",
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

impl bindings::osagent::browser::browser::Host for HostState {
    fn open_session(
        &mut self,
        _options: bindings::osagent::browser::browser::SessionOptions,
    ) -> Result<Resource<bindings::osagent::browser::browser::Session>, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }
}

impl bindings::osagent::browser::browser::HostSession for HostState {
    fn close(&mut self, _rep: Resource<bindings::osagent::browser::browser::Session>) {}

    fn drop(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }

    fn goto(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _url: String,
        _timeout_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<bindings::osagent::browser::browser::PageState, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn describe_page(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _include_html: bool,
    ) -> Result<bindings::osagent::browser::browser::PageState, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn screenshot(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _kind: bindings::osagent::browser::browser::ScreenshotKind,
    ) -> Result<bindings::osagent::browser::browser::Screenshot, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn eval(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _expression: String,
    ) -> Result<bindings::osagent::common::types::Json, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn find(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _selector: bindings::osagent::browser::browser::Selector,
        _timeout_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<Resource<bindings::osagent::browser::browser::ElementHandle>, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn query_all(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::Session>,
        _selector: bindings::osagent::browser::browser::Selector,
    ) -> Result<Vec<Resource<bindings::osagent::browser::browser::ElementHandle>>, CapabilityError>
    {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }
}

impl bindings::osagent::browser::browser::HostElementHandle for HostState {
    fn click(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn type_text(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
        _text: String,
        _submit: bool,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn clear(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn attribute(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
        _name: String,
    ) -> Result<Option<String>, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn inner_text(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
    ) -> Result<String, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn html(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
    ) -> Result<String, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "browser capability is not implemented",
        ))
    }

    fn drop(
        &mut self,
        _rep: Resource<bindings::osagent::browser::browser::ElementHandle>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl bindings::osagent::input::input::Host for HostState {
    fn key_sequence(&mut self, _text: String) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input capability is not implemented",
        ))
    }

    fn send_key_chord(
        &mut self,
        _chord: bindings::osagent::input::input::KeyChord,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input capability is not implemented",
        ))
    }

    fn mouse_move(
        &mut self,
        _motion: bindings::osagent::input::input::PointerMove,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input capability is not implemented",
        ))
    }

    fn mouse_click(
        &mut self,
        _button: bindings::osagent::input::input::MouseButton,
        _hold_ms: Option<bindings::osagent::common::types::Milliseconds>,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input capability is not implemented",
        ))
    }

    fn mouse_scroll(
        &mut self,
        _delta: bindings::osagent::input::input::ScrollDelta,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "input capability is not implemented",
        ))
    }
}

impl bindings::osagent::llm::llm::Host for HostState {
    fn complete(
        &mut self,
        messages: Vec<bindings::osagent::llm::llm::Message>,
        options: bindings::osagent::llm::llm::Options,
    ) -> Result<bindings::osagent::llm::llm::CompletionResponse, CapabilityError> {
        let settings = require_llm_settings(&self.config)?;
        let request = build_chat_request(&settings.model, messages, options, None);
        let response = execute_chat_request(settings, &request)?;
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
        let settings = require_llm_settings(&self.config)?;
        let chat_tools = tools_to_chat(tools)?;
        let request = build_chat_request(&settings.model, messages, options, Some(chat_tools));
        let response = execute_chat_request(settings, &request)?;
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

impl bindings::osagent::policy::policy::Host for HostState {
    fn describe(
        &mut self,
    ) -> Result<bindings::osagent::policy::policy::PolicySnapshot, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "policy capability is not implemented",
        ))
    }

    fn claim_budget(
        &mut self,
        _kind: bindings::osagent::policy::policy::BudgetKind,
        _units: u64,
    ) -> Result<bindings::osagent::policy::policy::BudgetSnapshot, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "policy capability is not implemented",
        ))
    }

    fn request_capability(
        &mut self,
        _request: bindings::osagent::policy::policy::GrantRequest,
    ) -> Result<bindings::osagent::policy::policy::GrantResponse, CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "policy capability is not implemented",
        ))
    }

    fn log_event(
        &mut self,
        _event: bindings::osagent::common::types::AuditEvent,
    ) -> Result<(), CapabilityError> {
        Err(capability_error(
            CapabilityErrorCode::Denied,
            "policy capability is not implemented",
        ))
    }
}
