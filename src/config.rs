use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub llm: LlmConfig,
    pub gmail: GmailConfig,
    pub storage: StorageConfig,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_style: String,
    pub orchestrator_model: String,
    pub worker_model: String,
    pub max_context_tokens: usize,
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GmailConfig {
    pub client_secret_path: String,
    pub redirect_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub sqlite_path: String,
    pub log_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_supervisor_steps: usize,
    pub max_worker_steps: usize,
    pub max_tool_retries: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            gmail: GmailConfig::default(),
            storage: StorageConfig::default(),
            agent: AgentConfig::default(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".into(),
            api_key: "ollama".into(),
            api_style: "auto".into(),
            orchestrator_model: "qwen2.5:14b".into(),
            worker_model: "qwen2.5:14b".into(),
            max_context_tokens: 8192,
            request_timeout_secs: 300,
        }
    }
}

impl Default for GmailConfig {
    fn default() -> Self {
        Self {
            client_secret_path: "~/.config/agentic-cli/client_secret.json".into(),
            redirect_port: 8765,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            sqlite_path: "~/.local/share/agentic-cli/harness.db".into(),
            log_path: "~/.local/share/agentic-cli/harness.log".into(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_supervisor_steps: 12,
            max_worker_steps: 8,
            max_tool_retries: 2,
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Self> {
        let location = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_config_path(),
        };
        if location.exists() {
            let text = std::fs::read_to_string(&location)
                .with_context(|| format!("reading {}", location.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", location.display()))
        } else if explicit.is_some() {
            anyhow::bail!("config not found: {}", location.display());
        } else {
            Ok(Self::default())
        }
    }

    pub fn sqlite_path(&self) -> PathBuf {
        expand_home(&self.storage.sqlite_path)
    }

    pub fn log_path(&self) -> PathBuf {
        expand_home(&self.storage.log_path)
    }

    pub fn client_secret_path(&self) -> PathBuf {
        expand_home(&self.gmail.client_secret_path)
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.gmail.redirect_port)
    }
}

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/agentic-cli")
}

pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

pub fn init_tracing(log_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(file)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    Ok(())
}

pub fn write_example_config() -> anyhow::Result<PathBuf> {
    let path = default_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        anyhow::bail!("config already exists at {}", path.display());
    }
    std::fs::write(&path, include_str!("../config.example.toml"))?;
    Ok(path)
}
