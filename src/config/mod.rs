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

/// The reserved route name used when a request's content does not clear the
/// classification threshold for any other route (or is not classified at
/// all — a build without the `semantic` feature, an unloaded classifier).
/// `validate` requires this route to exist; every other route is an ordinary
/// classification candidate.
pub const DEFAULT_ROUTE: &str = "default";

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

    /// Names that clients ask for. Wildcard names (a `*` anywhere in the
    /// key) are rejected by `validate` — every route name matches exactly.
    #[serde(default)]
    pub routes: BTreeMap<String, RouteConfig>,

    /// The target for Claude Code's own internal `<transcript>`-prefixed
    /// auto-mode judgment requests — see `crate::server::proxy::classify_request`.
    /// Deliberately separate from `routes`: it never depends on a route name or
    /// the client's requested model string, only on what the operator pins here,
    /// so it can be pointed at a fast target regardless of what model name the
    /// client's internal classifier happens to ask for. `None` falls back to
    /// resolving by the requested model name, then `default`, same as before
    /// this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<ModelConfig>,

    /// Per-client launch tweaks (`llm-gateway launch claude|codex|opencode`).
    /// Every field has a sane built-in default, so a normal `config.json`
    /// omits this key entirely — it exists only for the rare hand-edit
    /// (extra CLI args, Codex's `wireApi`, opencode's provider overrides).
    #[serde(default, skip_serializing_if = "LaunchConfig::is_empty")]
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
/// A route's default and fallbacks may speak different variants — crossing
/// variants needs request/response translation (see `crate::translate`), and
/// `crate::server::proxy` decides per target whether that translation exists,
/// dropping the targets it does not reach rather than rejecting the route at
/// config-validation time.
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

/// How a provider is reached.
///
/// Almost always HTTP. `claude-cli` runs the local Claude Code binary instead,
/// which is the only way a *subscription* can serve gateway traffic: a Claude
/// Pro/Max plan authenticates that client, not any credential the gateway could
/// present upstream. See [`crate::agent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Http,
    /// `claude -p`, driven as a subprocess.
    ClaudeCli,
    /// `codex exec`, driven as a subprocess.
    CodexCli,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::ClaudeCli => "claude-cli",
            Self::CodexCli => "codex-cli",
        }
    }

    /// Whether a `baseUrl` means anything for this transport.
    pub fn needs_base_url(self) -> bool {
        matches!(self, Self::Http)
    }

    /// Whether this transport runs a local agent CLI rather than making a
    /// request.
    pub fn is_agent_cli(self) -> bool {
        matches!(self, Self::ClaudeCli | Self::CodexCli)
    }

    /// The protocol a transport's output is in. `None` for HTTP, where the
    /// provider's `api` is whatever it says it is.
    pub fn fixed_api(self) -> Option<ApiKind> {
        match self {
            Self::Http => None,
            // The CLI emits Anthropic stream events verbatim.
            Self::ClaudeCli => Some(ApiKind::AnthropicMessages),
            // Codex emits its own event stream, which the gateway renders as
            // `openai-chat` — the protocol the most clients can reach, and the
            // one Claude Code can be translated into.
            Self::CodexCli => Some(ApiKind::OpenaiChat),
        }
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfig {
    /// Without a trailing slash, e.g. `https://openrouter.ai/api/v1`.
    ///
    /// Optional only because a `claude-cli` provider has no URL to speak of;
    /// `validate` requires it for every HTTP provider.
    #[serde(default)]
    pub base_url: String,

    pub api: ApiKind,

    /// How to reach the provider. Defaults to HTTP, so every existing config
    /// keeps its meaning.
    #[serde(default, skip_serializing_if = "is_http")]
    pub transport: Transport,

    /// Extra arguments appended to an agent CLI's command line. An escape hatch
    /// for the flags this gateway does not model (`--add-dir`, a different
    /// `--permission-mode`); ignored by HTTP providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_args: Vec<String>,

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

fn is_http(transport: &Transport) -> bool {
    *transport == Transport::Http
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Human-readable purpose, inline or as a path to a Markdown file — or
    /// several of either, one per language a route's traffic actually
    /// arrives in.
    ///
    /// This is documentation *and* the classification corpus: every request,
    /// regardless of what model name the client sent, is classified against
    /// every route's `description` and dispatched to whichever route's text
    /// is the closest match. A route with no description cannot win — see
    /// `validate`, which requires one on every route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Description>,

    pub model: ModelConfig,
}

/// A `description` value: either the text itself, a path to a file holding
/// it, or a list of either. Accepted in `config.json` as a bare string or as
/// an array; a bare string normalizes to a single-entry list, so an existing
/// config keeps its exact meaning (see [`Description::deserialize`]).
///
/// More than one entry exists for a route whose traffic mixes languages. The
/// embedding model behind classification (`potion-multilingual-128M`) aligns
/// weakly across languages, and writing one description that mixes languages
/// dilutes each of them rather than covering both — measured: a
/// Japanese-only description scores 0.550 cosine against a Japanese query,
/// while appending an English clause to that same description drops it to
/// 0.433. `crate::semantic::index` embeds every entry separately and scores
/// a route by the **maximum** cosine to any of them, so each language gets
/// its own undiluted embedding instead of being averaged away.
///
/// Each entry follows the same path-or-text rule individually: treated as a
/// path when it starts with `./`, `../`, `/` or `~/`, literal text
/// otherwise, so ordinary prose can never be mistaken for a filename.
#[derive(Debug, Clone)]
pub struct Description(pub Vec<String>);

impl Description {
    /// Every configured variant, in the order they were written.
    pub fn variants(&self) -> &[String] {
        &self.0
    }

    /// Resolved filesystem paths of every variant that is a path reference —
    /// what `validate` checks for existence, one per variant.
    ///
    /// Relative paths resolve against the config directory rather than the
    /// process working directory, so config.json stays portable.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.0
            .iter()
            .filter_map(|variant| variant_path(variant))
            .collect()
    }

    /// The text of every variant, reading any that reference a file.
    ///
    /// Used by `crate::semantic::index` as the classification corpus, one
    /// embedding per entry; the contract each path entry relies on (the
    /// referenced file must exist) is already checked by `validate`.
    pub fn texts(&self) -> Result<Vec<String>> {
        self.0.iter().map(|variant| variant_text(variant)).collect()
    }
}

fn variant_is_path(variant: &str) -> bool {
    let s = variant.trim();
    s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/")
}

fn variant_path(variant: &str) -> Option<PathBuf> {
    if !variant_is_path(variant) {
        return None;
    }
    let s = variant.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(strategy) = etcetera::choose_base_strategy() {
            use etcetera::BaseStrategy;
            return Some(strategy.home_dir().join(rest));
        }
    }
    Some(paths::resolve_relative(s))
}

fn variant_text(variant: &str) -> Result<String> {
    match variant_path(variant) {
        None => Ok(variant.to_string()),
        Some(path) => {
            std::fs::read_to_string(&path).map_err(|source| Error::ConfigRead { path, source })
        }
    }
}

/// Accepts either a bare string or an array of strings, normalizing a bare
/// string to a single-entry `Vec` — the shape every existing config already
/// has, so nothing written before this type grew a second variant needs to
/// change.
impl<'de> serde::Deserialize<'de> for Description {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::One(s) => Description(vec![s]),
            Raw::Many(v) => Description(v),
        })
    }
}

/// A single variant serializes as a bare string rather than a one-element
/// array — what `init` writes for `DescriptionLanguage::English`, and what
/// keeps a hand-edited single-language config reading exactly as before.
impl Serialize for Description {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0.as_slice() {
            [one] => serializer.serialize_str(one),
            many => many.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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

impl LaunchConfig {
    fn is_empty(&self) -> bool {
        self.claude.is_none() && self.codex.is_none() && self.opencode.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchClaude {
    /// Appended to the child's argv before any user-supplied arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchCodex {
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchOpencode {
    /// Route names exposed to opencode. Each must appear in `GET /v1/models`
    /// verbatim — opencode fails silently on a mismatch rather than erroring.
    /// Empty means "every route".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,

    /// Built-in opencode providers whose `baseURL` is redirected to the
    /// gateway at launch.
    ///
    /// opencode picks a provider from the `provider/` prefix of each model
    /// reference, so an agent file pinning `model: openai/gpt-…` would
    /// otherwise talk to OpenAI directly and silently bypass the gateway.
    /// Redirecting the built-in providers keeps per-agent model choices
    /// intact while routing every request through the gateway. Set to `[]`
    /// to disable.
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

    /// Whether `serve` prints its diagnostic console log (embedding-model
    /// preparation, which route/provider was picked, per-attempt fallback
    /// outcomes, ...) to stderr.
    ///
    /// Off by default: a plain gateway process should stay quiet, and other
    /// tooling/agents driving `serve` should not have their stderr suddenly
    /// grow noisier on upgrade. Setting it to `true` raises the console log
    /// level so those `tracing::info!` calls are actually emitted; it does
    /// not affect the on-disk usage/trace logs controlled by [`Self::usage`]
    /// and [`Self::debug`] above, and an explicit `RUST_LOG` still wins over
    /// it.
    #[serde(default)]
    pub logging: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
            usage: true,
            debug: false,
            logging: false,
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

    /// Route names, for advertising in `GET /v1/models`.
    pub fn listable_routes(&self) -> Vec<&str> {
        self.routes.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `logging.logging` defaults to `false` — a plain gateway process stays
    /// quiet unless the user opts into the console diagnostics.
    #[test]
    fn logging_console_flag_defaults_to_false() {
        assert!(!LoggingConfig::default().logging);
    }

    /// An existing `config.json` written before this field existed must keep
    /// parsing, with the console log staying off — the whole point of
    /// `#[serde(default)]` here.
    #[test]
    fn logging_console_flag_is_optional_in_json() {
        let parsed: LoggingConfig = json5::from_str(r#"{ "dir": "./logs" }"#).unwrap();
        assert!(!parsed.logging);
    }

    #[test]
    fn logging_console_flag_can_be_turned_on() {
        let parsed: LoggingConfig =
            json5::from_str(r#"{ "dir": "./logs", "logging": true }"#).unwrap();
        assert!(parsed.logging);
    }

    /// A bare string is the shape every existing config already has — it
    /// must normalize to a single-entry list, not be rejected or produce a
    /// list of characters.
    #[test]
    fn description_string_normalizes_to_a_single_variant() {
        let d: Description = json5::from_str(r#""hello""#).unwrap();
        assert_eq!(d.variants(), &["hello".to_string()]);
    }

    #[test]
    fn description_array_stays_as_written() {
        let d: Description = json5::from_str(r#"["日本語の説明", "English description"]"#).unwrap();
        assert_eq!(
            d.variants(),
            &[
                "日本語の説明".to_string(),
                "English description".to_string()
            ]
        );
    }

    /// The whole point of accepting a bare string on read: a single-variant
    /// `Description` must write back out as a bare string too, so a config
    /// hand-edited or regenerated by `init` for `DescriptionLanguage::English`
    /// reads exactly as it did before this type grew a second variant.
    #[test]
    fn single_variant_serializes_as_a_bare_string() {
        let d = Description(vec!["hello".to_string()]);
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"hello\"");
    }

    #[test]
    fn multi_variant_serializes_as_an_array() {
        let d = Description(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(serde_json::to_string(&d).unwrap(), "[\"a\",\"b\"]");
    }

    #[test]
    fn texts_reads_a_path_entry_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("desc.md");
        std::fs::write(&file_path, "file contents").unwrap();

        let d = Description(vec![file_path.to_string_lossy().to_string()]);
        assert_eq!(d.texts().unwrap(), vec!["file contents".to_string()]);
    }

    /// A route can mix an inline variant with a path variant — each entry is
    /// resolved independently.
    #[test]
    fn texts_resolves_a_mix_of_inline_and_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("desc.md");
        std::fs::write(&file_path, "from file").unwrap();

        let d = Description(vec![
            "inline text".to_string(),
            file_path.to_string_lossy().to_string(),
        ]);
        assert_eq!(
            d.texts().unwrap(),
            vec!["inline text".to_string(), "from file".to_string()]
        );
    }
}
