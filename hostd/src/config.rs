use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::budget;
use crate::cli::StepArgs;
use crate::sandbox::{self, AllowedProgram};
use crate::secrets::Secrets;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub workspace_root: Utf8PathBuf,
    /// The allowlist as configured (file and command line merged), by name.
    pub allowed_proc_commands: Vec<String>,
    /// The allowlist resolved when the config loaded: what each entry runs as.
    pub allowed_programs: Vec<AllowedProgram>,
    /// The environment variables a plan may set for a spawned process ([`sandbox::check_env_key`]).
    pub allowed_env: Vec<String>,
    /// The `PATH` the host gives a spawned process ([`sandbox::child_path`]).
    pub child_path: OsString,
    pub llm: Option<LlmSettings>,
    pub browser: Option<BrowserSettings>,
    /// The run's budgets (`[budgets]`).
    pub budgets: budget::Limits,
    /// The rules for the workspace (`[workspace]`).
    pub workspace_rules: WorkspaceRules,
    /// Host-side secrets (`[secrets]`), named in plans as `{{secret:NAME}}`.
    pub secrets: Secrets,
    /// Where the audit log is appended (`[audit] path`), if anywhere.
    pub audit_path: Option<Utf8PathBuf>,
}

/// `[workspace]`: what the plan may do to the workspace.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceRules {
    /// Refuse every write: creating, writing, truncating, renaming and removing.
    pub read_only: bool,
    /// The largest a file may be made by a write (`write`, `write-string` and `set-len`, appends
    /// included). Reads are capped by the host alone (`sandbox::cap_read`).
    pub max_file_bytes: u64,
}

impl Default for WorkspaceRules {
    fn default() -> Self {
        Self {
            read_only: false,
            max_file_bytes: 16 << 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmSettings {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct BrowserSettings {
    pub webdriver_url: String,
    /// Read and ignored: profiles are not implemented, and every session starts fresh.
    #[allow(dead_code)]
    pub default_profile: Option<String>,
    /// The hosts the browser may visit (`*.example.com` for subdomains). Empty allows nothing.
    pub allowed_hosts: Vec<String>,
    /// Launch Chrome with `--no-sandbox` (only for hosts where its sandbox cannot run). Off by default.
    pub chrome_no_sandbox: bool,
}

impl HostConfig {
    pub fn from_step_args(args: &StepArgs) -> Result<Self> {
        let file_cfg = FileConfig::load(&args.config)?;
        let workspace_path = args
            .workspace
            .clone()
            .or_else(|| file_cfg.workspace_root.clone().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let workspace_root = normalize_path(&workspace_path).with_context(|| {
            format!(
                "invalid workspace path {}",
                workspace_path.to_string_lossy()
            )
        })?;
        let mut allowed_proc_commands = file_cfg.allow_proc.unwrap_or_default();
        allowed_proc_commands.extend(args.allow_proc.iter().cloned());
        allowed_proc_commands.sort();
        allowed_proc_commands.dedup();
        let path_var = std::env::var_os("PATH");
        let allowed_programs = allowed_proc_commands
            .iter()
            .map(|entry| sandbox::resolve_allowed(entry, path_var.as_deref(), &workspace_root))
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::msg)?;
        let child_path = sandbox::child_path(path_var.as_deref(), &workspace_root);
        let allowed_env = file_cfg.allow_env.unwrap_or_default();
        let llm = match file_cfg.llm {
            Some(cfg) => cfg.into_settings()?,
            None => None,
        };
        let browser = match file_cfg.browser {
            Some(cfg) => cfg.into_settings()?,
            None => None,
        };
        Ok(Self {
            workspace_root,
            allowed_proc_commands,
            allowed_programs,
            allowed_env,
            child_path,
            llm,
            browser,
            budgets: file_cfg.budgets.unwrap_or_default().into_limits(),
            workspace_rules: file_cfg.workspace.unwrap_or_default().into_rules(),
            secrets: load_secrets(file_cfg.secrets.unwrap_or_default())?,
            audit_path: file_cfg
                .audit
                .and_then(|a| a.path)
                .map(|p| normalize_path(Path::new(&p)))
                .transpose()?,
        })
    }

    /// What `requested` runs as, if the allowlist admits it ([`sandbox::admit`]).
    pub fn program_for(&self, requested: &str) -> Option<&Path> {
        sandbox::admit(&self.allowed_programs, requested)
    }
}

/// hostd.toml. An unknown key or section is an error, so a misspelt rule (`[workspaces]`) is not
/// silently dropped.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    workspace_root: Option<String>,
    allow_proc: Option<Vec<String>>,
    allow_env: Option<Vec<String>>,
    llm: Option<LlmFileSettings>,
    browser: Option<BrowserFileSettings>,
    budgets: Option<BudgetFileSettings>,
    workspace: Option<WorkspaceFileSettings>,
    secrets: Option<std::collections::BTreeMap<String, SecretSource>>,
    audit: Option<AuditFileSettings>,
}

/// One `[secrets]` entry: the value itself, or the environment variable holding it.
#[derive(Deserialize)]
#[serde(untagged)]
enum SecretSource {
    Value(String),
    Env { env: String },
}

fn load_secrets(entries: std::collections::BTreeMap<String, SecretSource>) -> Result<Secrets> {
    let mut values = Vec::new();
    for (name, source) in entries {
        let value = match source {
            SecretSource::Value(value) => value,
            SecretSource::Env { env } => std::env::var(&env).with_context(|| {
                format!("secret `{name}`: environment variable {env} is not set")
            })?,
        };
        values.push((name, value));
    }
    Ok(Secrets::new(values))
}

/// `[audit]`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditFileSettings {
    path: Option<String>,
}

/// `[budgets]`; each limit falls back to [`budget::Limits::default`].
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFileSettings {
    steps: Option<u64>,
    wallclock_ms: Option<u64>,
    llm_tokens: Option<u64>,
    processes: Option<u64>,
    filesystem_ops: Option<u64>,
    browser_actions: Option<u64>,
    guest_fuel: Option<u64>,
    guest_memory_bytes: Option<u64>,
}

impl BudgetFileSettings {
    fn into_limits(self) -> budget::Limits {
        let d = budget::Limits::default();
        budget::Limits {
            steps: self.steps.unwrap_or(d.steps),
            wallclock_ms: self.wallclock_ms.unwrap_or(d.wallclock_ms),
            llm_tokens: self.llm_tokens.unwrap_or(d.llm_tokens),
            processes: self.processes.unwrap_or(d.processes),
            filesystem_ops: self.filesystem_ops.unwrap_or(d.filesystem_ops),
            browser_actions: self.browser_actions.unwrap_or(d.browser_actions),
            guest_fuel: self.guest_fuel.unwrap_or(d.guest_fuel),
            guest_memory_bytes: self.guest_memory_bytes.unwrap_or(d.guest_memory_bytes),
        }
    }
}

/// `[workspace]`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceFileSettings {
    read_only: Option<bool>,
    max_file_bytes: Option<u64>,
}

impl WorkspaceFileSettings {
    fn into_rules(self) -> WorkspaceRules {
        let d = WorkspaceRules::default();
        WorkspaceRules {
            read_only: self.read_only.unwrap_or(d.read_only),
            max_file_bytes: self.max_file_bytes.unwrap_or(d.max_file_bytes),
        }
    }
}

impl FileConfig {
    fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse config {}", path.display()))
        } else {
            Ok(Self::default())
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmFileSettings {
    api_base: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
}

impl LlmFileSettings {
    fn into_settings(self) -> Result<Option<LlmSettings>> {
        let api_key = match self.api_key {
            Some(key) if !key.trim().is_empty() => key,
            _ => return Ok(None),
        };
        let model = match self.model {
            Some(model) if !model.trim().is_empty() => model,
            _ => return Ok(None),
        };
        let api_base = self
            .api_base
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        Ok(Some(LlmSettings {
            api_base,
            api_key,
            model,
        }))
    }
}

fn normalize_path(path: &Path) -> Result<Utf8PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read current directory")?
            .join(path)
    };
    let canonical = fs::canonicalize(&absolute).unwrap_or(absolute);
    Utf8PathBuf::from_path_buf(canonical).map_err(|_| anyhow::anyhow!("path is not valid UTF-8"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserFileSettings {
    webdriver_url: Option<String>,
    default_profile: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    chrome_no_sandbox: Option<bool>,
}

impl BrowserFileSettings {
    fn into_settings(self) -> Result<Option<BrowserSettings>> {
        let url = match self.webdriver_url {
            Some(url) if !url.trim().is_empty() => url,
            _ => return Ok(None),
        };
        Ok(Some(BrowserSettings {
            webdriver_url: url,
            default_profile: self.default_profile.filter(|p| !p.trim().is_empty()),
            allowed_hosts: self.allowed_hosts.unwrap_or_default(),
            chrome_no_sandbox: self.chrome_no_sandbox.unwrap_or(false),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(config: &Path) -> StepArgs {
        StepArgs {
            component: PathBuf::from("agent.wasm"),
            config: config.to_path_buf(),
            workspace: None,
            task: "t".into(),
            observation: "{}".into(),
            step: 0,
            allow_proc: Vec::new(),
        }
    }

    fn write(name: &str, body: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "hostd-config-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hostd.toml");
        fs::write(&path, body).expect("write config");
        path
    }

    #[test]
    fn a_missing_config_file_gives_safe_defaults() {
        let cfg = HostConfig::from_step_args(&args(Path::new("/nonexistent/hostd.toml"))).unwrap();
        assert!(cfg.allowed_proc_commands.is_empty(), "no command may run");
        assert!(cfg.llm.is_none() && cfg.browser.is_none());
        assert_eq!(
            cfg.workspace_root.as_std_path(),
            fs::canonicalize(".").unwrap()
        );
    }

    #[test]
    fn the_command_line_overrides_and_extends_the_file() {
        let dir = std::env::temp_dir();
        let path = write(
            "merge",
            "workspace_root = \"/does/not/matter\"\nallow_proc = [\"ls\", \"echo\"]\n",
        );
        let mut a = args(&path);
        a.workspace = Some(dir.clone());
        a.allow_proc = vec!["echo".into(), "cat".into()];
        let cfg = HostConfig::from_step_args(&a).unwrap();
        assert_eq!(
            cfg.workspace_root.as_std_path(),
            fs::canonicalize(&dir).unwrap()
        );
        assert_eq!(
            cfg.allowed_proc_commands,
            ["cat", "echo", "ls"],
            "merged, sorted, deduplicated"
        );
    }

    #[test]
    fn an_llm_section_without_a_key_or_a_model_is_off() {
        for body in [
            "[llm]\nmodel = \"m\"\n",
            "[llm]\napi_key = \"k\"\n",
            "[llm]\napi_key = \" \"\nmodel = \"m\"\n",
        ] {
            let cfg = HostConfig::from_step_args(&args(&write("llmoff", body))).unwrap();
            assert!(cfg.llm.is_none(), "{body}");
        }
    }

    #[test]
    fn the_llm_endpoint_defaults_to_openai() {
        let cfg = HostConfig::from_step_args(&args(&write(
            "llmdefault",
            "[llm]\napi_key = \"k\"\nmodel = \"m\"\n",
        )))
        .unwrap();
        assert_eq!(cfg.llm.unwrap().api_base, "https://api.openai.com/v1");
    }

    #[test]
    fn a_browser_section_needs_a_webdriver_url() {
        let on = HostConfig::from_step_args(&args(&write(
            "browseron",
            "[browser]\nwebdriver_url = \"http://127.0.0.1:9515\"\n",
        )))
        .unwrap();
        assert_eq!(on.browser.unwrap().webdriver_url, "http://127.0.0.1:9515");
        let off = HostConfig::from_step_args(&args(&write(
            "browseroff",
            "[browser]\ndefault_profile = \"p\"\n",
        )))
        .unwrap();
        assert!(off.browser.is_none());
    }

    #[test]
    fn a_malformed_config_is_an_error() {
        let path = write("bad", "allow_proc = \"ls\"\n");
        assert!(HostConfig::from_step_args(&args(&path)).is_err());
    }

    /// A misspelt section or key is an error, not a rule silently dropped.
    #[test]
    fn an_unknown_section_or_key_is_an_error() {
        for body in [
            "[workspaces]\nread_only = true\n",
            "alow_proc = []\n",
            "[llm]\nmodle = \"m\"\n",
            "[browser]\nallowed_host = []\n",
        ] {
            let path = write("unknown", body);
            let err = HostConfig::from_step_args(&args(&path))
                .err()
                .unwrap_or_else(|| panic!("accepted: {body}"));
            assert!(format!("{err:#}").contains("unknown"), "{err:#}");
        }
    }

    /// The sample configuration, as shipped, loads (its workspace, `./sandbox`, is relative to
    /// the repository root, so the test supplies one).
    #[test]
    fn the_sample_config_loads() {
        let sample = include_str!("../../hostd.toml.example");
        let path = write("sample", sample);
        let mut args = args(&path);
        args.workspace = path.parent().map(Path::to_path_buf);
        if let Err(err) = HostConfig::from_step_args(&args) {
            panic!("{err:#}");
        }
    }

    #[test]
    fn an_empty_allowlist_allows_nothing() {
        let cfg = HostConfig::from_step_args(&args(Path::new("/nonexistent/hostd.toml"))).unwrap();
        assert!(cfg.program_for("ls").is_none());
        assert!(cfg.program_for("").is_none());
    }

    #[test]
    fn the_allowlist_resolves_at_load_and_a_missing_program_is_an_error() {
        let cfg = HostConfig::from_step_args(&args(&write("resolve", "allow_proc = [\"echo\"]\n")))
            .unwrap();
        let echo = cfg.program_for("echo").expect("echo resolves");
        assert!(echo.is_absolute(), "{}", echo.display());
        let missing = write("missing", "allow_proc = [\"no-such-program-xyz\"]\n");
        assert!(HostConfig::from_step_args(&args(&missing)).is_err());
    }

    #[test]
    fn the_browser_allows_no_host_and_keeps_its_sandbox_by_default() {
        let cfg = HostConfig::from_step_args(&args(&write(
            "browserdefaults",
            "[browser]\nwebdriver_url = \"http://127.0.0.1:9515\"\n",
        )))
        .unwrap();
        let browser = cfg.browser.unwrap();
        assert!(browser.allowed_hosts.is_empty());
        assert!(!browser.chrome_no_sandbox);
    }
}
