use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::cli::StepArgs;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub workspace_root: Utf8PathBuf,
    pub allowed_proc_commands: Vec<String>,
    pub llm: Option<LlmSettings>,
    pub browser: Option<BrowserSettings>,
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
    pub default_profile: Option<String>,
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
            llm,
            browser,
        })
    }

    pub fn is_proc_allowed(&self, program: &str) -> bool {
        if self.allowed_proc_commands.is_empty() {
            return false;
        }
        let base = Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        self.allowed_proc_commands
            .iter()
            .any(|entry| entry == program || entry == base)
    }
}

#[derive(Default, Deserialize)]
struct FileConfig {
    workspace_root: Option<String>,
    allow_proc: Option<Vec<String>>,
    llm: Option<LlmFileSettings>,
    browser: Option<BrowserFileSettings>,
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
struct BrowserFileSettings {
    webdriver_url: Option<String>,
    default_profile: Option<String>,
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

    #[test]
    fn an_empty_allowlist_allows_nothing() {
        let cfg = HostConfig::from_step_args(&args(Path::new("/nonexistent/hostd.toml"))).unwrap();
        assert!(!cfg.is_proc_allowed("ls"));
        assert!(!cfg.is_proc_allowed(""));
    }
}
