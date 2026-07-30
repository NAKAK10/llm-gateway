//! Configuration data model and loading.
//!
//! `config.json` is parsed as **JSON5**, so comments, trailing commas and
//! unquoted keys are allowed. The extension stays `.json` because that is what
//! editors and `jq`-adjacent tooling expect, and because the shape deliberately
//! mirrors `openclaw.json`.
//!
//! The file contains API keys. It is created `chmod 600` and `config check`
//! complains if the mode drifts.

pub mod secret;
pub mod validate;
pub mod watch;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths;

pub use secret::SecretRef;

/// Root of `config.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    /// Upstream providers, keyed by an arbitrary id used in `model` strings.
    /// The same upstream may appear under several ids to expose it via
    /// different protocols (e.g. `openrouter` and `openrouter-anthropic`).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,

    /// Names that clients ask for. Either an exact name or a `*`-suffixed
    /// wildcard such as `claude-*`.
    #[serde(default)]
    pub routes: BTreeMap<String, RouteConfig>,

    #[serde(default)]
    pub launch: LaunchConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    /// Inbound bearer token clients must present.
    ///
    /// Optional only while `host` is a loopback address. Binding to anything
    /// reachable from the network without a key would hand every configured
    /// provider to whoever finds the port, so that combination is refused at
    /// startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            api_key: None,
        }
    }
}

impl ServerConfig {
    /// `http://127.0.0.1:4000` — what clients are pointed at.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Whether `host` is loopback-only. Uses string comparison rather than
    /// `IpAddr::is_loopback` so that `localhost` (which may resolve to `::1`)
    /// is treated conservatively.
    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "::1" | "localhost")
    }
}

fn default_host() -> String {
    // 127.0.0.1 rather than localhost: localhost can resolve to ::1, and a
    // v4-only listener then rejects clients that "correctly" used localhost.
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    4000
}

/// Wire protocol a provider speaks.
///
/// Fallback targets must stay within one variant for now — crossing variants
/// needs request/response translation, which is deliberately out of scope until
/// the passthrough path is proven. `validate` enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    /// `POST {base_url}/chat/completions`
    OpenaiChat,
    /// `POST {base_url}/responses`
    OpenaiResponses,
    /// `POST {base_url}/v1/messages`
    AnthropicMessages,
}

impl ApiKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }
}

impl std::fmt::Display for ApiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfig {
    /// Without a trailing slash, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,

    pub api: ApiKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,

    /// Extra headers sent upstream. OpenRouter's `HTTP-Referer` / `X-Title`
    /// live here; both are optional and only affect its public rankings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Add `stream_options.include_usage` to streaming `openai-chat` requests.
    ///
    /// Without it the upstream never reports token counts for streamed
    /// responses and cost accounting silently reads zero. It appends one extra
    /// usage-only chunk, so it can be turned off for clients that choke on it.
    #[serde(default = "default_true")]
    pub inject_usage: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Human-readable purpose, inline or as a path to a Markdown file.
    ///
    /// This is documentation today and the classification corpus once semantic
    /// routing lands — the more concrete it is, the better that will work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Description>,

    pub model: ModelConfig,
}

/// A `description` value: either the text itself or a path to a file holding it.
///
/// Treated as a path when it starts with `./`, `../`, `/` or `~/`. Everything
/// else is literal text, so ordinary prose can never be mistaken for a filename.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Description(pub String);

impl Description {
    pub fn is_path(&self) -> bool {
        let s = self.0.trim();
        s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/")
    }

    /// Resolved filesystem path, if this is a path reference.
    ///
    /// Relative paths resolve against the config directory rather than the
    /// process working directory, so config.json stays portable.
    pub fn path(&self) -> Option<PathBuf> {
        if !self.is_path() {
            return None;
        }
        let s = self.0.trim();
        if let Some(rest) = s.strip_prefix("~/") {
            if let Ok(strategy) = etcetera::choose_base_strategy() {
                use etcetera::BaseStrategy;
                return Some(strategy.home_dir().join(rest));
            }
        }
        Some(paths::resolve_relative(s))
    }

    /// The description text, reading the referenced file when needed.
    ///
    /// Unused until semantic routing (Phase 2) embeds these as the
    /// classification corpus; kept because it defines the contract `validate`
    /// already checks (the referenced file must exist).
    #[allow(dead_code)]
    pub fn text(&self) -> Result<String> {
        match self.path() {
            None => Ok(self.0.clone()),
            Some(path) => {
                std::fs::read_to_string(&path).map_err(|source| Error::ConfigRead { path, source })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfig {
    /// `"<provider>/<model>"`, e.g. `ollama-cloud/qwen3.5:397b`.
    ///
    /// Split on the **first** `/` only, so both a colon in the model name
    /// (`glm-5.2:cloud`) and OpenRouter's own `vendor/model` form
    /// (`openrouter/anthropic/claude-sonnet-4.6`) parse correctly.
    pub default: String,

    /// Tried in order, and only before the first response byte reaches the
    /// client. Once bytes are streaming the status line is already sent and
    /// switching upstreams is impossible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<LaunchClaude>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<LaunchCodex>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<LaunchOpencode>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchClaude {
    /// Route name passed as `ANTHROPIC_MODEL`.
    pub model: String,

    /// Appended to the child's argv before any user-supplied arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchCodex {
    /// Route name passed via `-c model="..."`.
    pub model: String,

    /// `responses` or `chat`.
    ///
    /// Sources disagree on whether Codex still accepts `chat`. The gateway
    /// serves both endpoints, so this is a switch rather than a guess.
    #[serde(default = "default_wire_api")]
    pub wire_api: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

fn default_wire_api() -> String {
    "responses".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchOpencode {
    /// Route name; becomes `-m gateway/<model>`.
    pub model: String,

    /// Route names exposed to opencode. Each must appear in `GET /v1/models`
    /// verbatim — opencode fails silently on a mismatch rather than erroring.
    /// Empty means "every non-wildcard route".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,

    /// Built-in opencode providers whose `baseURL` is redirected to the
    /// gateway at launch.
    ///
    /// opencode picks a provider from the `provider/` prefix of each model
    /// reference, so an agent file pinning `model: openai/gpt-…` would
    /// otherwise talk to OpenAI directly and silently bypass the gateway.
    /// Redirecting the built-in providers keeps per-agent model choices
    /// intact while routing every request through the gateway (the `gpt-*` /
    /// `claude-*` wildcard routes forward the ids unchanged). Set to `[]` to
    /// disable.
    #[serde(default = "default_opencode_overrides")]
    pub override_providers: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

fn default_opencode_overrides() -> Vec<String> {
    vec!["openai".to_string(), "anthropic".to_string()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoggingConfig {
    /// Relative paths resolve against the config directory.
    #[serde(default = "default_log_dir")]
    pub dir: String,

    /// One JSONL line per request in `usage-YYYY-MM.jsonl`.
    #[serde(default = "default_true")]
    pub usage: bool,

    /// Full routing decisions in `trace-YYYY-MM-DD.jsonl`.
    ///
    /// Records prompt text (truncated unless `--debug-full`), so it is off by
    /// default. `--debug` on the command line overrides this.
    #[serde(default)]
    pub debug: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
            usage: true,
            debug: false,
        }
    }
}

fn default_log_dir() -> String {
    "./logs".to_string()
}

/// A parsed `"<provider>/<model>"` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    /// May contain further `/` (OpenRouter) and `:` (Ollama).
    pub model: String,
}

impl ModelRef {
    /// Split on the first `/` only.
    ///
    /// `openrouter/anthropic/claude-sonnet-4.6` → provider `openrouter`,
    /// model `anthropic/claude-sonnet-4.6`.
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, model) = s.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }

    /// Substitute `*` in the model part with the model the client asked for.
    ///
    /// This is what lets `claude-*` → `anthropic/*` forward
    /// `claude-sonnet-4-6` untouched, so client-side model names never have to
    /// be rewritten and stay correct as vendors publish new ids.
    pub fn expand(&self, requested: &str) -> Self {
        Self {
            provider: self.provider.clone(),
            model: self.model.replace('*', requested),
        }
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

impl Config {
    /// Load and validate the default config file.
    pub fn load() -> Result<Self> {
        Self::load_from(&paths::config_file())
    }

    /// Load and validate a specific file.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = Self::read(path)?;
        let report = validate::validate(&raw, path);
        if !report.is_ok() {
            return Err(Error::ConfigInvalid(report));
        }
        Ok(raw)
    }

    /// Parse without validating. Used by `config check`, which wants to report
    /// every problem rather than stop at the first one.
    pub fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigMissing(path.to_path_buf()));
        }
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        json5::from_str(&text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    /// Route names that are safe to advertise in `GET /v1/models`.
    ///
    /// Wildcards are excluded: they are forwarding rules, not selectable
    /// models, and listing them would let a client pick the literal string
    /// `claude-*`.
    pub fn listable_routes(&self) -> Vec<&str> {
        self.routes
            .keys()
            .filter(|k| !k.contains('*'))
            .map(|k| k.as_str())
            .collect()
    }
}
