//! `llm-gateway init` — write a first config.
//!
//! Asks three questions (clients, providers, how to store keys), writes a
//! minimal `config.json` plus matching `llm/*.md` description stubs, and stops.
//! Everything after that is hand-edited; the wizard is a starting point, not a
//! settings UI.
//!
//! Two rules it does not break:
//!
//! - **It never touches a client's config file.** Redirecting a client is
//!   `launch`'s job, at launch time.
//! - **It never overwrites an existing `config.json`.** It shows what it would
//!   have written and exits.
//!
//! The file is created `0600` because the literal-key option puts real
//! credentials in it.

use crate::config::SecretRef;
use crate::error::Result;
use crate::paths;

/// Permissions for `config.json`. It can hold API keys in the clear.
pub const CONFIG_MODE: u32 = 0o600;

/// Stub written to `llm/role-default.md` for the sample `role-default` route.
const ROLE_DEFAULT_STUB: &str = "\
# role-default

Sample route description. Edit this file to describe when `role-default`
should be picked once semantic routing lands; today it is just documentation.
";

pub fn run() -> Result<()> {
    let config_path = paths::config_file();
    if config_path.exists() {
        println!("config already exists at {}", config_path.display());
        println!("edit it directly — `init` never overwrites an existing config.json");
        return Ok(());
    }

    cliclack::intro("llm-gateway init")?;

    let clients: Vec<Client> = cliclack::multiselect("Which clients do you use?")
        .item(Client::Claude, "Claude Code", "")
        .item(Client::Codex, "Codex CLI", "")
        .item(Client::Opencode, "opencode", "")
        .initial_values(vec![Client::Claude])
        .interact()?;

    let mut provider_select = cliclack::multiselect("Which providers do you want to configure?");
    for provider in KnownProvider::ALL {
        provider_select = provider_select.item(provider, provider.label(), provider.base_url());
    }
    let selected_providers: Vec<KnownProvider> = provider_select
        .initial_values(vec![KnownProvider::Anthropic, KnownProvider::OpenRouter])
        .interact()?;

    let storage = cliclack::select("How should API keys be stored?")
        .item(
            KeyStorage::Literal,
            "In config.json (chmod 600)",
            "simplest",
        )
        .item(KeyStorage::Env, "Environment variable", "${VAR}")
        .item(
            KeyStorage::Keychain,
            "macOS Keychain",
            "keychain:<id>, macOS only",
        )
        .interact()?;

    // A password left empty falls back to the `${VAR}` form for that one
    // provider, even though the overall storage choice is Literal — typing a
    // real key is optional, not doing so should not write an empty string.
    let mut providers: Vec<(KnownProvider, Option<String>)> = Vec::new();
    let mut env_fallback: Vec<KnownProvider> = Vec::new();
    for provider in &selected_providers {
        let literal = if storage == KeyStorage::Literal && provider.needs_key() {
            let value = cliclack::password(format!("API key for {}", provider.label()))
                .mask('*')
                .allow_empty()
                .interact()?;
            if value.is_empty() {
                env_fallback.push(*provider);
                None
            } else {
                Some(value)
            }
        } else {
            None
        };
        providers.push((*provider, literal));
    }

    let mut config = build_config(&clients, &providers, storage);
    for provider in &env_fallback {
        let key = SecretRef::new(format!("${{{}}}", provider.env_var()));
        if let Some(provider_config) = config.providers.get_mut(provider.id()) {
            provider_config.api_key = Some(key.clone());
        }
        if *provider == KnownProvider::OpenRouter {
            if let Some(provider_config) = config.providers.get_mut("openrouter-anthropic") {
                provider_config.api_key = Some(key);
            }
        }
    }

    let dir = paths::config_dir();
    let llm_dir = paths::llm_dir();
    let logs_dir = paths::logs_dir(&config.logging.dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(&llm_dir)?;
    std::fs::create_dir_all(&logs_dir)?;

    let json = serde_json::to_string_pretty(&config)?;
    let contents = format!("// llm-gateway config — do not commit this file\n{json}\n");
    std::fs::write(&config_path, contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&config_path)?.permissions();
        perms.set_mode(CONFIG_MODE);
        std::fs::set_permissions(&config_path, perms)?;
    }

    let role_default_path = llm_dir.join("role-default.md");
    if !role_default_path.exists() {
        std::fs::write(&role_default_path, ROLE_DEFAULT_STUB)?;
    }

    let first_launch = clients.first().copied().unwrap_or(Client::Claude);
    cliclack::outro(format!(
        "wrote {}\n\nnext steps:\n  llm-gateway config check\n  llm-gateway serve\n  llm-gateway launch {}",
        config_path.display(),
        first_launch.launch_name(),
    ))?;

    Ok(())
}

/// A client the wizard can scaffold a `launch` entry (and matching routes) for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Claude,
    Codex,
    Opencode,
}

impl Client {
    /// Name accepted by `llm-gateway launch <name>`.
    pub fn launch_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

/// A provider the wizard knows how to scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProvider {
    Anthropic,
    OpenAi,
    OpenRouter,
    Gemini,
    Xai,
    Mistral,
    DeepSeek,
    Groq,
    TogetherAi,
    SakanaAi,
    Plamo,
    OllamaCloud,
    OllamaLocal,
}

impl KnownProvider {
    /// Every provider the wizard offers, in menu order.
    pub const ALL: [KnownProvider; 13] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
        Self::Gemini,
        Self::Xai,
        Self::Mistral,
        Self::DeepSeek,
        Self::Groq,
        Self::TogetherAi,
        Self::SakanaAi,
        Self::Plamo,
        Self::OllamaCloud,
        Self::OllamaLocal,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::Gemini => "gemini",
            Self::Xai => "xai",
            Self::Mistral => "mistral",
            Self::DeepSeek => "deepseek",
            Self::Groq => "groq",
            Self::TogetherAi => "together",
            Self::SakanaAi => "sakana",
            Self::Plamo => "plamo",
            Self::OllamaCloud => "ollama-cloud",
            Self::OllamaLocal => "ollama-local",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::Gemini => "Google Gemini",
            Self::Xai => "xAI (Grok)",
            Self::Mistral => "Mistral",
            Self::DeepSeek => "DeepSeek",
            Self::Groq => "Groq",
            Self::TogetherAi => "Together AI",
            Self::SakanaAi => "Sakana AI",
            Self::Plamo => "PLaMo (Preferred Networks)",
            Self::OllamaCloud => "Ollama Cloud",
            Self::OllamaLocal => "Ollama (local)",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
            Self::Xai => "https://api.x.ai/v1",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::TogetherAi => "https://api.together.xyz/v1",
            Self::SakanaAi => "https://api.sakana.ai/v1",
            Self::Plamo => "https://api.platform.preferredai.jp/v1",
            Self::OllamaCloud => "https://ollama.com/v1",
            Self::OllamaLocal => "http://127.0.0.1:11434/v1",
        }
    }

    pub fn api(self) -> crate::config::ApiKind {
        use crate::config::ApiKind;
        match self {
            Self::Anthropic => ApiKind::AnthropicMessages,
            Self::OpenAi => ApiKind::OpenaiResponses,
            Self::OpenRouter
            | Self::Gemini
            | Self::Xai
            | Self::Mistral
            | Self::DeepSeek
            | Self::Groq
            | Self::TogetherAi
            | Self::SakanaAi
            | Self::Plamo
            | Self::OllamaCloud
            | Self::OllamaLocal => ApiKind::OpenaiChat,
        }
    }

    /// Environment variable name used by the `${VAR}` option.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::Xai => "XAI_API_KEY",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::Groq => "GROQ_API_KEY",
            Self::TogetherAi => "TOGETHER_API_KEY",
            Self::SakanaAi => "SAKANA_API_KEY",
            Self::Plamo => "PLAMO_API_KEY",
            Self::OllamaCloud => "OLLAMA_API_KEY",
            Self::OllamaLocal => "OLLAMA_LOCAL_KEY",
        }
    }

    /// Local endpoints do not need a real credential, so the wizard does not ask
    /// for one.
    pub fn needs_key(self) -> bool {
        !matches!(self, Self::OllamaLocal)
    }
}

/// How keys should be referenced in the generated file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStorage {
    /// Written into `config.json` (which is created `0600`).
    Literal,
    /// `"${VAR}"`.
    Env,
    /// `"keychain:<id>"`.
    Keychain,
}

/// Build the config the wizard's answers imply.
///
/// Pure, so the generated shape is testable without a terminal.
pub fn build_config(
    clients: &[Client],
    providers: &[(KnownProvider, Option<String>)],
    storage: KeyStorage,
) -> crate::config::Config {
    use crate::config::{ApiKind, Config, ModelConfig, ProviderConfig, RouteConfig};

    let selected: Vec<KnownProvider> = providers.iter().map(|(p, _)| *p).collect();
    let has = |p: KnownProvider| selected.contains(&p);
    let literal_for = |p: KnownProvider| {
        providers
            .iter()
            .find(|(candidate, _)| *candidate == p)
            .and_then(|(_, literal)| literal.clone())
    };

    let mut config = Config::default();

    for (provider, literal) in providers {
        config.providers.insert(
            provider.id().to_string(),
            ProviderConfig {
                base_url: provider.base_url().to_string(),
                api: provider.api(),
                api_key: Some(provider_api_key(*provider, literal.clone(), storage)),
                headers: Default::default(),
                inject_usage: true,
            },
        );
    }

    // OpenRouter can also speak the Anthropic wire protocol under
    // `openrouter/anthropic/*`; expose it under its own id so `claude-*` can
    // fall back to it without crossing `ApiKind`s.
    if has(KnownProvider::OpenRouter) {
        config.providers.insert(
            "openrouter-anthropic".to_string(),
            ProviderConfig {
                base_url: KnownProvider::OpenRouter.base_url().to_string(),
                api: ApiKind::AnthropicMessages,
                api_key: Some(provider_api_key(
                    KnownProvider::OpenRouter,
                    literal_for(KnownProvider::OpenRouter),
                    storage,
                )),
                headers: Default::default(),
                inject_usage: true,
            },
        );
    }

    if has(KnownProvider::Anthropic) && clients.contains(&Client::Claude) {
        let mut fallbacks = Vec::new();
        if has(KnownProvider::OpenRouter) {
            fallbacks.push("openrouter-anthropic/anthropic/*".to_string());
        }
        config.routes.insert(
            "claude-*".to_string(),
            RouteConfig {
                model: ModelConfig {
                    default: "anthropic/*".to_string(),
                    fallbacks,
                },
                ..Default::default()
            },
        );
    }

    if has(KnownProvider::OpenAi) && clients.contains(&Client::Codex) {
        let mut fallbacks = Vec::new();
        if has(KnownProvider::OpenRouter) {
            fallbacks.push("openrouter/openai/*".to_string());
        }
        config.routes.insert(
            "gpt-*".to_string(),
            RouteConfig {
                model: ModelConfig {
                    default: "openai/*".to_string(),
                    fallbacks,
                },
                ..Default::default()
            },
        );
    }

    if let Some((first, _)) = providers.first() {
        config.routes.insert(
            "role-default".to_string(),
            RouteConfig {
                model: ModelConfig {
                    default: format!("{}/*", first.id()),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    for client in clients {
        match client {
            Client::Claude => config.launch.claude = Some(launch_claude()),
            Client::Codex => config.launch.codex = Some(launch_codex()),
            Client::Opencode => config.launch.opencode = Some(launch_opencode()),
        }
    }

    config
}

fn launch_claude() -> crate::config::LaunchClaude {
    crate::config::LaunchClaude {
        model: "claude-sonnet-4-6".to_string(),
        extra_args: Vec::new(),
    }
}

fn launch_codex() -> crate::config::LaunchCodex {
    crate::config::LaunchCodex {
        model: "gpt-5.6".to_string(),
        wire_api: "responses".to_string(),
        extra_args: Vec::new(),
    }
}

fn launch_opencode() -> crate::config::LaunchOpencode {
    crate::config::LaunchOpencode {
        // `role-default` always exists — the wizard writes it for the first
        // selected provider. Empty `models` = every non-wildcard route.
        model: "role-default".to_string(),
        models: Vec::new(),
        override_providers: vec!["openai".to_string(), "anthropic".to_string()],
        extra_args: Vec::new(),
    }
}

/// The `apiKey` value for one provider, given how keys should be stored.
///
/// Providers that [`KnownProvider::needs_key`] does not require (local
/// endpoints) always get the literal `"local"`, regardless of `storage`.
fn provider_api_key(
    provider: KnownProvider,
    literal: Option<String>,
    storage: KeyStorage,
) -> SecretRef {
    if !provider.needs_key() {
        return SecretRef::new("local");
    }

    match storage {
        KeyStorage::Literal => SecretRef::new(literal.unwrap_or_default()),
        KeyStorage::Env => SecretRef::new(format!("${{{}}}", provider.env_var())),
        KeyStorage::Keychain => SecretRef::new(format!("keychain:{}", provider.id())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_route_gets_openrouter_anthropic_fallback() {
        let config = build_config(
            &[Client::Claude],
            &[
                (KnownProvider::Anthropic, Some("sk-ant-test".to_string())),
                (KnownProvider::OpenRouter, Some("sk-or-test".to_string())),
            ],
            KeyStorage::Literal,
        );

        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("openrouter-anthropic"));

        let route = config.routes.get("claude-*").expect("claude-* route");
        assert_eq!(route.model.default, "anthropic/*");
        assert_eq!(
            route.model.fallbacks,
            vec!["openrouter-anthropic/anthropic/*".to_string()]
        );

        assert!(!config.routes.contains_key("gpt-*"));
    }

    #[test]
    fn codex_only_with_openai_has_no_fallback_without_openrouter() {
        let config = build_config(
            &[Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Literal,
        );

        let route = config.routes.get("gpt-*").expect("gpt-* route");
        assert_eq!(route.model.default, "openai/*");
        assert!(route.model.fallbacks.is_empty());

        assert!(!config.routes.contains_key("claude-*"));
        assert!(!config.providers.contains_key("openrouter-anthropic"));
    }

    #[test]
    fn unselected_provider_gets_no_route() {
        let config = build_config(
            &[Client::Claude, Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Env,
        );

        assert!(!config.routes.contains_key("claude-*"));
        assert!(config.routes.contains_key("gpt-*"));
    }

    #[test]
    fn literal_storage_uses_the_given_value() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, Some("sk-ant-abc".to_string()))],
            KeyStorage::Literal,
        );
        let provider = config.providers.get("anthropic").unwrap();
        assert_eq!(provider.api_key.as_ref().unwrap().0, "sk-ant-abc");
    }

    #[test]
    fn env_storage_references_the_known_variable() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Env,
        );
        let provider = config.providers.get("anthropic").unwrap();
        assert_eq!(provider.api_key.as_ref().unwrap().0, "${ANTHROPIC_API_KEY}");
    }

    #[test]
    fn keychain_storage_references_the_provider_id() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
        );
        let provider = config.providers.get("anthropic").unwrap();
        assert_eq!(provider.api_key.as_ref().unwrap().0, "keychain:anthropic");
    }

    #[test]
    fn local_provider_key_is_always_literal_local() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::OllamaLocal, None)],
            KeyStorage::Literal,
        );
        let provider = config.providers.get("ollama-local").unwrap();
        assert_eq!(provider.api_key.as_ref().unwrap().0, "local");
    }
}
