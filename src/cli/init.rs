//! `llm-gateway init` — write a first config.
//!
//! Asks three questions (clients, providers, how to store keys), writes a
//! `config.json` with one classifiable route per selected provider plus the
//! reserved `default` route, downloads the embedding model classification
//! needs, and stops. Everything after that is hand-edited; the wizard is a
//! starting point, not a settings UI.
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

/// Download and verify the embedding model classification needs, so a fresh
/// install already has it before the first request ever arrives — rather
/// than surprising whoever sends that request with a ~500MB download.
///
/// A no-op message in a build without the `semantic` feature: there is no
/// model to fetch, and every request will resolve to the reserved `default`
/// route without classification (see `crate::server::prepare_classifier`).
#[cfg(feature = "semantic")]
async fn ensure_classification_model() -> Result<()> {
    cliclack::log::info(
        "downloading the classification model (potion-multilingual-128M, ~500MB) if not already \
         cached — this is what lets every request be routed by content...",
    )?;
    let http = crate::upstream::client()?;
    crate::semantic::model_file::ensure(&http).await?;
    cliclack::log::info("classification model ready")?;
    Ok(())
}

#[cfg(not(feature = "semantic"))]
async fn ensure_classification_model() -> Result<()> {
    cliclack::log::warning(
        "this build does not have the `semantic` feature enabled; every request will be routed \
         to the reserved `default` route without content-based classification",
    )?;
    Ok(())
}

pub async fn run() -> Result<()> {
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

    // Asked before the storage question, because a subscription answer means
    // there is no key to store for that provider at all.
    let mut subscriptions: Vec<KnownProvider> = Vec::new();
    for provider in &selected_providers {
        let Some(cli) = provider.subscription_cli() else {
            continue;
        };
        if !command_exists(cli) {
            continue;
        }
        let choice = cliclack::select(format!("{}: how do you pay for it?", provider.label()))
            .item(
                AuthChoice::ApiKey,
                "API key",
                "per-token billing; full API features",
            )
            .item(
                AuthChoice::Subscription,
                format!("Subscription (via `{cli}`)"),
                "no key; generation only — your tools are not passed through",
            )
            .interact()?;
        if choice == AuthChoice::Subscription {
            subscriptions.push(*provider);
        }
    }

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

    // A credential another installed tool already holds is better read from
    // that tool than copied out of it: `gh` refreshes its token on its own
    // schedule, so a copy would work today and 401 next week.
    let discovered: Vec<(KnownProvider, SecretRef)> = selected_providers
        .iter()
        .filter_map(|provider| provider.discovered_key().map(|key| (*provider, key)))
        .collect();

    // A password left empty falls back to the `${VAR}` form for that one
    // provider, even though the overall storage choice is Literal — typing a
    // real key is optional, not doing so should not write an empty string.
    let mut providers: Vec<(KnownProvider, Option<String>)> = Vec::new();
    let mut env_fallback: Vec<KnownProvider> = Vec::new();
    for provider in &selected_providers {
        let already_discovered = discovered
            .iter()
            .any(|(candidate, _)| candidate == provider)
            // …nor for one whose credential is a subscription: there is nothing
            // to type.
            || subscriptions.contains(provider);
        let literal =
            if storage == KeyStorage::Literal && provider.needs_key() && !already_discovered {
                // The hint matters: a subscription user has no API key to paste
                // here, and without being told that empty is allowed the wizard
                // reads as "an API key is mandatory for every provider".
                let value = cliclack::password(format!(
                    "API key for {} (empty → reference ${{{}}} instead)",
                    provider.label(),
                    provider.env_var(),
                ))
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

    let mut config = build_config_with_auth(&clients, &providers, storage, &subscriptions);
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
    for (provider, key) in &discovered {
        if let Some(provider_config) = config.providers.get_mut(provider.id()) {
            provider_config.api_key = Some(key.clone());
        }
        // Said out loud because it is the one key the user was not asked for —
        // silently wiring a credential would be worse than one extra line.
        cliclack::log::info(format!(
            "{}: reading the token from `{}` on each request, so a refresh needs no config change",
            provider.label(),
            key.masked(),
        ))?;
    }

    for provider in &subscriptions {
        cliclack::log::info(format!(
            "{}: `{}` route added, run by `{}` on your subscription — no key needed",
            provider.label(),
            provider.subscription_route(),
            provider.subscription_cli().unwrap_or("its CLI"),
        ))?;
    }

    let dir = paths::config_dir();
    let llm_dir = paths::llm_dir();
    let logs_dir = paths::logs_dir(&config.logging.dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(&llm_dir)?;
    std::fs::create_dir_all(&logs_dir)?;

    ensure_classification_model().await?;

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
    GithubCopilot,
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
    pub const ALL: [KnownProvider; 14] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
        Self::GithubCopilot,
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
            Self::GithubCopilot => "github-copilot",
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
            Self::GithubCopilot => "GitHub Copilot",
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
            Self::GithubCopilot => "https://api.githubcopilot.com",
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
            | Self::GithubCopilot
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
            // Not `GITHUB_TOKEN`: that name is set to something unrelated in
            // every CI environment, and a repo token silently 403s here.
            Self::GithubCopilot => "GITHUB_COPILOT_TOKEN",
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

    /// Extra request headers this provider wants, beyond auth.
    ///
    /// Only GitHub Copilot has any. It answers a bare
    /// `Authorization: Bearer <github token>` perfectly well, so this is not a
    /// requirement — pinning `X-GitHub-Api-Version` just means a future default
    /// change on GitHub's side cannot alter the shape of the responses
    /// mid-session.
    ///
    /// Deliberately *not* sent: `x-initiator` and `Openai-Intent`. Copilot uses
    /// them to classify traffic, and their correct value depends on the
    /// individual request (whether the last turn came from the human or from a
    /// tool loop). A gateway-wide constant would be wrong half the time, and a
    /// wrong classification is worse than none.
    pub fn headers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::GithubCopilot => &[("X-GitHub-Api-Version", "2026-06-01")],
            _ => &[],
        }
    }

    /// The credential reference to suggest when the user has a tool that already
    /// holds one, instead of asking them to paste a key.
    ///
    /// GitHub Copilot is the case that needs this: the credential is an ordinary
    /// GitHub token that `gh` already has and refreshes on its own, so the right
    /// answer is to read it from `gh` on every attempt rather than copy it.
    /// Returns `None` when the tool is not installed, so the wizard falls back
    /// to the normal key question.
    pub fn discovered_key(self) -> Option<SecretRef> {
        match self {
            Self::GithubCopilot if command_exists("gh") => {
                Some(SecretRef::new("command:gh auth token"))
            }
            _ => None,
        }
    }

    /// Classification text for the route this provider is scaffolded with.
    ///
    /// Every request is classified against every route's description now (see
    /// `crate::semantic::index`), so this needs to say something a request's
    /// own wording could plausibly match — not just restate the provider's
    /// name. Deliberately generic: it is a starting point the user is
    /// expected to edit once they know their own usage pattern, not a
    /// finished taxonomy.
    pub fn description(self) -> &'static str {
        match self {
            Self::Anthropic => {
                "Complex reasoning, coding, and multi-step agentic tasks that need careful \
                 step-by-step thinking and full tool support."
            }
            Self::OpenAi => {
                "General-purpose assistant tasks, coding, and tool use via OpenAI's models."
            }
            Self::OpenRouter => {
                "Wide catalog of open and third-party models; use for specific model requests \
                 or as a fallback when a primary provider is unavailable."
            }
            Self::GithubCopilot => {
                "Coding assistance and pair-programming tasks via GitHub Copilot's models."
            }
            Self::Gemini => "Multimodal tasks and long-context questions via Google Gemini.",
            Self::Xai => {
                "Questions about current events and real-time information via xAI's Grok models."
            }
            Self::Mistral => "Lightweight, general-purpose tasks via Mistral's models.",
            Self::DeepSeek => "Cost-efficient reasoning and coding tasks via DeepSeek's models.",
            Self::Groq => {
                "Latency-sensitive, simple tasks that benefit from Groq's fast inference."
            }
            Self::TogetherAi => "Open-source models hosted via Together AI.",
            Self::SakanaAi => "Experimental or research-oriented tasks via Sakana AI's models.",
            Self::Plamo => {
                "Japanese-language tasks and general assistance via Preferred Networks' PLaMo."
            }
            Self::OllamaCloud => "General-purpose tasks via Ollama Cloud's hosted open models.",
            Self::OllamaLocal => {
                "Private, offline inference where no data should leave this machine, via a \
                 locally running Ollama."
            }
        }
    }
}

/// Whether `name` is an executable on `PATH`.
///
/// `command -v` is a shell builtin, so this goes through `sh` — which also makes
/// it agree with how `command:` secret references are run.
fn command_exists(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

impl KnownProvider {
    /// The agent-CLI transport that serves this provider's *subscription*, if
    /// one exists.
    ///
    /// This is what makes "API key or subscription?" a real question rather than
    /// a preference: a Claude Pro/Max or ChatGPT plan authenticates its own CLI,
    /// so the gateway runs that CLI instead of holding a credential. See
    /// [`crate::agent`].
    pub fn subscription_transport(self) -> Option<crate::config::Transport> {
        use crate::config::Transport;
        match self {
            Self::Anthropic => Some(Transport::ClaudeCli),
            Self::OpenAi => Some(Transport::CodexCli),
            _ => None,
        }
    }

    /// The CLI a subscription choice depends on, for the wizard to check before
    /// offering it.
    pub fn subscription_cli(self) -> Option<&'static str> {
        match self.subscription_transport()? {
            crate::config::Transport::ClaudeCli => Some(crate::agent::claude_cli::PROGRAM),
            crate::config::Transport::CodexCli => Some(crate::agent::codex_cli::PROGRAM),
            crate::config::Transport::Http => None,
        }
    }

    /// Provider id used when the subscription option is chosen. Distinct from
    /// [`Self::id`] so a config can hold both — a plan for interactive routes and
    /// an API key for the ones that need tools.
    pub fn subscription_id(self) -> String {
        format!("{}-subscription", self.id())
    }

    /// Route name for this provider's own classifiable route.
    pub fn route_name(self) -> String {
        format!("role-{}", self.id())
    }

    /// Route name for the subscription choice. Namespaced by provider id so
    /// selecting both Anthropic and OpenAI subscriptions scaffolds two routes
    /// instead of the second silently overwriting the first.
    pub fn subscription_route(self) -> String {
        format!("role-{}-subscription", self.id())
    }

    /// A model reference the subscription CLI accepts, for the scaffolded route.
    pub fn subscription_model(self) -> &'static str {
        match self {
            // The CLI resolves aliases, so this keeps working across releases.
            Self::Anthropic => "sonnet",
            // Codex has no alias, and which models a ChatGPT plan allows is
            // not knowable here — `default` means "whatever the CLI is
            // configured to use".
            _ => "default",
        }
    }
}

/// Which credential a provider should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthChoice {
    /// An API key, in whatever form [`KeyStorage`] says.
    ApiKey,
    /// The provider's own CLI, authenticated by a subscription.
    Subscription,
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
    build_config_with_auth(clients, providers, storage, &[])
}

/// [`build_config`], plus the providers whose credential is a subscription
/// rather than a key.
///
/// A subscription choice adds a *second* provider id (`<id>-subscription`) with
/// an agent-CLI transport, and a `role-<id>-subscription` route pointing at
/// it. It
/// does not remove the API-key provider: the two are good at different things —
/// a plan for generation, a key for anything that needs tools — and a config
/// that holds both lets a route choose per request.
pub fn build_config_with_auth(
    _clients: &[Client],
    providers: &[(KnownProvider, Option<String>)],
    storage: KeyStorage,
    subscriptions: &[KnownProvider],
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
                headers: provider
                    .headers()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                inject_usage: true,
                transport: Default::default(),
                agent_args: Vec::new(),
            },
        );
    }

    // OpenRouter can also speak the Anthropic wire protocol under
    // `openrouter/anthropic/*`; expose it under its own id so Anthropic's
    // classifiable route can fall back to it without crossing `ApiKind`s.
    //
    // Its Anthropic-compatible root is `/api`, not `/api/v1` — unlike the
    // `openai-chat` id, which needs the `/v1` prefix because the gateway
    // appends `/chat/completions` rather than a version-free path. Reusing
    // `KnownProvider::OpenRouter.base_url()` here would double up to
    // `/api/v1/v1/messages`.
    if has(KnownProvider::OpenRouter) {
        config.providers.insert(
            "openrouter-anthropic".to_string(),
            ProviderConfig {
                base_url: "https://openrouter.ai/api".to_string(),
                api: ApiKind::AnthropicMessages,
                api_key: Some(provider_api_key(
                    KnownProvider::OpenRouter,
                    literal_for(KnownProvider::OpenRouter),
                    storage,
                )),
                headers: Default::default(),
                inject_usage: true,
                transport: Default::default(),
                agent_args: Vec::new(),
            },
        );
    }

    for provider in providers.iter().map(|(p, _)| *p) {
        config.routes.insert(
            provider.route_name(),
            RouteConfig {
                description: Some(crate::config::Description(
                    provider.description().to_string(),
                )),
                model: ModelConfig {
                    default: format!("{}/*", provider.id()),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    if has(KnownProvider::Anthropic) && has(KnownProvider::OpenRouter) {
        // Both share the same `ApiKind`, so OpenRouter can serve as a
        // fallback on Anthropic's own classifiable route without crossing
        // protocols.
        if let Some(route) = config
            .routes
            .get_mut(&KnownProvider::Anthropic.route_name())
        {
            route
                .model
                .fallbacks
                .push("openrouter-anthropic/anthropic/*".to_string());
        }
    }

    if has(KnownProvider::OpenAi) && has(KnownProvider::OpenRouter) {
        if let Some(route) = config.routes.get_mut(&KnownProvider::OpenAi.route_name()) {
            route
                .model
                .fallbacks
                .push("openrouter/openai/*".to_string());
        }
    }

    // The reserved catch-all for whatever does not clear the classification
    // threshold on any other route — points at the first selected provider,
    // which is as good a default as any and always exists once at least one
    // provider was chosen.
    if let Some((first, _)) = providers.first() {
        config.routes.insert(
            crate::config::DEFAULT_ROUTE.to_string(),
            RouteConfig {
                description: Some(crate::config::Description(
                    "Fallback for requests that do not clearly match any other route's \
                     description."
                        .to_string(),
                )),
                model: ModelConfig {
                    default: format!("{}/*", first.id()),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    for provider in subscriptions {
        let Some(transport) = provider.subscription_transport() else {
            continue;
        };
        let Some(api) = transport.fixed_api() else {
            continue;
        };
        let id = provider.subscription_id();
        config.providers.insert(
            id.clone(),
            ProviderConfig {
                // Both are meaningless for a CLI transport: it has no URL, and
                // it authenticates itself.
                base_url: String::new(),
                api,
                api_key: None,
                headers: Default::default(),
                inject_usage: true,
                transport,
                agent_args: Vec::new(),
            },
        );
        config.routes.insert(
            provider.subscription_route(),
            RouteConfig {
                description: Some(crate::config::Description(
                    "Runs on a subscription via the provider's own CLI. Generation only — \
                     the caller's tools are not passed through."
                        .to_string(),
                )),
                model: ModelConfig {
                    default: format!("{id}/{}", provider.subscription_model()),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    // `launch.<client>` is deliberately left unset: every field it could
    // hold has a built-in default (see `crate::launch::run`), so writing it
    // here would only add noise to a config.json that should read as
    // `server` + `providers` + `routes` + `logging`. A user who wants
    // Codex's `wireApi: "chat"`, opencode's `overrideProviders`, or extra
    // launcher CLI args adds `launch` back by hand.

    config
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

    /// The scaffolded Copilot provider must be reachable from Claude Code,
    /// which means `openai-chat` plus the translation layer — the whole reason
    /// it can be offered at all.
    #[test]
    fn a_subscription_choice_scaffolds_a_cli_transport_and_a_route() {
        let config = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[KnownProvider::Anthropic],
        );

        let provider = config
            .providers
            .get("anthropic-subscription")
            .expect("subscription provider");
        assert_eq!(provider.transport, crate::config::Transport::ClaudeCli);
        assert_eq!(provider.api, crate::config::ApiKind::AnthropicMessages);
        // Neither means anything for a CLI transport, and validation warns when
        // they are set.
        assert!(provider.base_url.is_empty());
        assert!(provider.api_key.is_none());

        let route = config
            .routes
            .get("role-anthropic-subscription")
            .expect("route");
        assert_eq!(route.model.default, "anthropic-subscription/sonnet");
        assert!(route.description.is_some(), "the route explains its limits");
    }

    /// The API-key provider stays: a plan is good for generation and a key is
    /// good for tools, so a config that holds both lets routes choose.
    #[test]
    fn choosing_a_subscription_does_not_remove_the_api_key_provider() {
        let config = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[KnownProvider::Anthropic],
        );
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("anthropic-subscription"));
    }

    /// Choosing subscriptions for both Anthropic and OpenAI used to collide:
    /// both wrote to the fixed route name `role-subscription`, so the second
    /// silently overwrote the first. Each now gets its own `role-<id>-subscription`.
    #[test]
    fn two_subscriptions_scaffold_two_routes_instead_of_colliding() {
        let config = build_config_with_auth(
            &[Client::Claude, Client::Codex],
            &[
                (KnownProvider::Anthropic, None),
                (KnownProvider::OpenAi, None),
            ],
            KeyStorage::Keychain,
            &[KnownProvider::Anthropic, KnownProvider::OpenAi],
        );
        assert_eq!(
            config.routes["role-anthropic-subscription"].model.default,
            "anthropic-subscription/sonnet"
        );
        assert_eq!(
            config.routes["role-openai-subscription"].model.default,
            "openai-subscription/default"
        );
    }

    /// OpenRouter's Anthropic-compatible root is `/api`, not `/api/v1` — the
    /// gateway appends `/v1/messages` itself. Reusing the `openai-chat` id's
    /// base URL here used to double up to `/api/v1/v1/messages`.
    #[test]
    fn openrouter_anthropic_base_url_has_no_doubled_v1() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::OpenRouter, None)],
            KeyStorage::Keychain,
        );
        let provider = &config.providers["openrouter-anthropic"];
        assert_eq!(provider.base_url, "https://openrouter.ai/api");
        assert_eq!(
            crate::upstream::endpoint_url(
                &crate::route::Target {
                    transport: provider.transport,
                    agent_args: provider.agent_args.clone(),
                    model_ref: crate::config::ModelRef {
                        provider: "openrouter-anthropic".into(),
                        model: "anthropic/claude-sonnet-4.6".into(),
                    },
                    api: provider.api,
                    base_url: provider.base_url.clone(),
                    api_key: None,
                    headers: provider
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    inject_usage: provider.inject_usage,
                },
                false
            ),
            "https://openrouter.ai/api/v1/messages"
        );
    }

    #[test]
    fn a_codex_subscription_route_defers_the_model_to_the_cli() {
        // Which models a ChatGPT plan allows is not knowable from here.
        let config = build_config_with_auth(
            &[Client::Codex],
            &[(KnownProvider::OpenAi, None)],
            KeyStorage::Keychain,
            &[KnownProvider::OpenAi],
        );
        let provider = &config.providers["openai-subscription"];
        assert_eq!(provider.transport, crate::config::Transport::CodexCli);
        // Codex renders as openai-chat, which is what the most clients reach.
        assert_eq!(provider.api, crate::config::ApiKind::OpenaiChat);
        assert_eq!(
            config.routes["role-openai-subscription"].model.default,
            "openai-subscription/default"
        );
    }

    #[test]
    fn only_providers_with_a_subscription_cli_offer_the_choice() {
        assert!(KnownProvider::Anthropic.subscription_transport().is_some());
        assert!(KnownProvider::OpenAi.subscription_transport().is_some());
        for provider in KnownProvider::ALL {
            if matches!(provider, KnownProvider::Anthropic | KnownProvider::OpenAi) {
                continue;
            }
            assert!(
                provider.subscription_transport().is_none(),
                "{} has no subscription CLI",
                provider.id()
            );
        }
    }

    #[test]
    fn no_subscriptions_generates_exactly_what_it_used_to() {
        let with = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[],
        );
        let without = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
        );
        assert_eq!(
            serde_json::to_string(&with).unwrap(),
            serde_json::to_string(&without).unwrap()
        );
    }

    #[test]
    fn github_copilot_is_scaffolded_as_an_openai_chat_provider() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::GithubCopilot, None)],
            KeyStorage::Keychain,
        );

        let provider = config
            .providers
            .get("github-copilot")
            .expect("github-copilot provider");
        assert_eq!(provider.api, crate::config::ApiKind::OpenaiChat);
        assert_eq!(provider.base_url, "https://api.githubcopilot.com");
        assert_eq!(
            provider
                .headers
                .get("X-GitHub-Api-Version")
                .map(String::as_str),
            Some("2026-06-01")
        );
        // Traffic-classification headers are request-dependent; a constant
        // would be wrong half the time, so none is written.
        assert!(
            !provider.headers.contains_key("x-initiator"),
            "{:?}",
            provider.headers
        );
        assert!(
            !provider.headers.contains_key("Openai-Intent"),
            "{:?}",
            provider.headers
        );
    }

    #[test]
    fn only_copilot_gets_extra_headers() {
        let config = build_config(
            &[Client::Claude],
            &[
                (KnownProvider::Anthropic, None),
                (KnownProvider::OllamaLocal, None),
            ],
            KeyStorage::Keychain,
        );
        for id in ["anthropic", "ollama-local"] {
            assert!(
                config.providers[id].headers.is_empty(),
                "{id} should have no extra headers"
            );
        }
    }

    #[test]
    fn copilots_env_var_is_not_the_overloaded_github_token() {
        // `GITHUB_TOKEN` is set to an unrelated repo token in every CI
        // environment, which would 403 against Copilot and look like a bug.
        assert_eq!(
            KnownProvider::GithubCopilot.env_var(),
            "GITHUB_COPILOT_TOKEN"
        );
    }

    #[test]
    fn anthropic_route_gets_openrouter_anthropic_fallback() {
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

        let route = config
            .routes
            .get("role-anthropic")
            .expect("role-anthropic route");
        assert_eq!(route.model.default, "anthropic/*");
        assert_eq!(
            route.model.fallbacks,
            vec!["openrouter-anthropic/anthropic/*".to_string()]
        );

        assert!(!config.routes.contains_key("role-openai"));
    }

    #[test]
    fn codex_only_with_openai_has_no_fallback_without_openrouter() {
        let config = build_config(
            &[Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Literal,
        );

        let route = config.routes.get("role-openai").expect("role-openai route");
        assert_eq!(route.model.default, "openai/*");
        assert!(route.model.fallbacks.is_empty());

        assert!(!config.routes.contains_key("role-anthropic"));
        assert!(!config.providers.contains_key("openrouter-anthropic"));
    }

    #[test]
    fn unselected_provider_gets_no_route() {
        let config = build_config(
            &[Client::Claude, Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Env,
        );

        assert!(!config.routes.contains_key("role-anthropic"));
        assert!(config.routes.contains_key("role-openai"));
    }

    #[test]
    fn a_default_route_is_always_scaffolded_when_a_provider_is_selected() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, Some("sk-ant-test".to_string()))],
            KeyStorage::Literal,
        );

        let route = config
            .routes
            .get(crate::config::DEFAULT_ROUTE)
            .expect("default route");
        assert_eq!(route.model.default, "anthropic/*");
        assert!(route.description.is_some());
    }

    #[test]
    fn every_selected_provider_gets_a_description_for_classification() {
        let config = build_config(
            &[Client::Claude],
            &[
                (KnownProvider::Anthropic, Some("sk-ant-test".to_string())),
                (KnownProvider::OpenRouter, Some("sk-or-test".to_string())),
            ],
            KeyStorage::Literal,
        );

        for name in ["role-anthropic", "role-openrouter"] {
            let route = config.routes.get(name).unwrap_or_else(|| panic!("{name}"));
            assert!(
                route.description.is_some(),
                "{name} needs a description to be a classification candidate"
            );
        }
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

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn command_exists_agrees_with_the_shell() {
        assert!(command_exists("sh"), "sh must be on PATH");
        assert!(!command_exists("llm-gateway-definitely-not-a-real-binary"));
    }

    /// Only Copilot has a tool to discover a credential from; every other
    /// provider must fall through to the wizard's normal key question.
    #[test]
    fn no_other_provider_discovers_a_key() {
        for provider in KnownProvider::ALL {
            if provider == KnownProvider::GithubCopilot {
                continue;
            }
            assert!(
                provider.discovered_key().is_none(),
                "{} should not discover a key",
                provider.id()
            );
        }
    }

    /// Machine-dependent by nature, so this asserts the *shape* rather than
    /// which branch was taken: whatever comes back must be a `command:`
    /// reference that names `gh`, and nothing else.
    #[test]
    fn a_discovered_copilot_key_is_a_gh_command_reference() {
        if let Some(key) = KnownProvider::GithubCopilot.discovered_key() {
            assert_eq!(key.kind(), crate::config::secret::SecretKind::Command);
            assert!(key.raw().contains("gh auth token"), "{}", key.raw());
        }
    }
}
