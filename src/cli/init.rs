//! `llm-gateway init` — write a first config.
//!
//! Asks which providers to use, which agent *role* (manager, architect,
//! explorer, ...) each configured provider/model should serve, and how to
//! store keys; writes a `config.json` with one classifiable route per
//! assigned role plus the reserved `default` route, downloads the embedding
//! model classification needs, and stops. Everything after that is
//! hand-edited; the wizard is a starting point, not a settings UI.
//!
//! It does not ask which client(s) — Claude Code, Codex CLI, opencode — are
//! in use: all three are supported unconditionally, and `launch` lets a user
//! switch between them at any time without touching `config.json`.
//!
//! Routes are named after the *role* a request is doing (`role-architect`,
//! `role-reviewer`, ...), not the provider serving it — see [`AgentRole`].
//! `role-anthropic` or `role-github-copilot` says nothing about what a
//! request needs to be doing to reach that route once more than one provider
//! is configured; the role name is the actual classification distinction.
//!
//! One rule it does not break:
//!
//! - **It never touches a client's config file.** Redirecting a client is
//!   `launch`'s job, at launch time.
//!
//! Running it again on top of an existing `config.json` asks first — regenerating
//! replaces every provider, route and key currently in the file. A "yes" is
//! preceded by a `config.json.bak` copy of what was there, so a wrong answer is
//! still recoverable.
//!
//! The file is created `0600` because the literal-key option puts real
//! credentials in it.

use crate::config::SecretRef;
use crate::error::Result;
use crate::paths;

/// Permissions for `config.json`. It can hold API keys in the clear.
pub const CONFIG_MODE: u32 = 0o600;

/// Where to copy an existing `config.json` before regenerating it.
///
/// Timestamped (to the second) rather than a fixed `.bak` name, so running
/// `init` over an existing config more than once never silently discards an
/// earlier backup — each confirmed regeneration keeps its own copy.
fn backup_path_for(config_path: &std::path::Path) -> std::path::PathBuf {
    let stamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown-time".to_string())
        .replace(':', "-");
    let file_name = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.json");
    config_path.with_file_name(format!("{file_name}.{stamp}.bak"))
}

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

/// The credential to send a `GET /models` probe with, resolved from whatever
/// the wizard already knows about a provider — a typed literal, a discovered
/// `command:` reference actually run, or the environment variable it would
/// otherwise fall back to. `None` means the wizard has no usable credential
/// for this provider *in this process* (e.g. the user left the key blank and
/// has not exported the variable yet); callers fall back to a free-text
/// model prompt in that case rather than failing.
fn resolve_key_for_fetch(
    provider: KnownProvider,
    providers: &[(KnownProvider, Option<String>)],
    discovered: &[(KnownProvider, SecretRef)],
) -> Option<String> {
    if let Some((_, Some(literal))) = providers.iter().find(|(p, _)| *p == provider) {
        return Some(literal.clone());
    }
    if let Some((_, secret)) = discovered.iter().find(|(p, _)| *p == provider) {
        let command = secret.raw().strip_prefix("command:")?;
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (!token.is_empty()).then_some(token);
    }
    std::env::var(provider.env_var()).ok()
}

/// Fetch the model ids a provider currently offers, for the wizard to present
/// as a single-choice list instead of a free-text prompt.
///
/// Every provider here speaks (or is assumed to speak) the OpenAI-style
/// `GET /models` -> `{"data":[{"id": "..."}, ...]}` shape, except Anthropic,
/// which uses `GET /v1/models` with its own auth header. A local Ollama needs
/// no credential at all. Any failure — network, non-2xx, or an unexpected
/// body shape — is surfaced as `Err` so the caller can fall back to typing a
/// model name; this is a nicety, not something `init` should ever block on.
async fn fetch_models(provider: KnownProvider, key: Option<&str>) -> Result<Vec<String>> {
    use crate::error::Error;

    let http = crate::upstream::client()?;
    let url = match provider {
        KnownProvider::Anthropic => format!("{}/v1/models", provider.base_url()),
        _ => format!("{}/models", provider.base_url()),
    };

    let mut request = http.get(&url);
    match provider {
        KnownProvider::Anthropic => {
            if let Some(key) = key {
                request = request.header("x-api-key", key);
            }
            request = request.header("anthropic-version", "2023-06-01");
        }
        KnownProvider::OllamaLocal => {}
        _ => {
            if let Some(key) = key {
                request = request.header("Authorization", format!("Bearer {key}"));
            }
        }
    }
    for (name, value) in provider.headers() {
        request = request.header(*name, *value);
    }

    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(Error::Other(format!(
            "{} returned {}",
            url,
            response.status()
        )));
    }
    let body: serde_json::Value = response.json().await?;
    let entries = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Other(format!("unexpected response shape from {url}")))?;
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(Error::Other(format!("{url} listed no models")));
    }
    Ok(ids)
}

/// How many model choices to show at once before the selection prompt turns
/// into a scrollable list. Providers such as OpenRouter can list hundreds of
/// models, so this keeps the prompt within a typical terminal height.
const MODEL_SELECT_MAX_ROWS: usize = 10;

/// Ask which model backs one role's route, presenting a fetched model list as
/// a single-choice (radio) prompt when the wizard could reach the provider's
/// `/models` endpoint, and falling back to a free-text prompt — pre-filled
/// with [`KnownProvider::default_model`] — when it could not.
async fn choose_model(
    role: AgentRole,
    provider: KnownProvider,
    key: Option<&str>,
) -> Result<String> {
    let prompt = format!(
        "Model for the `{}` role (via {})",
        role.label(),
        provider.label()
    );
    match fetch_models(provider, key).await {
        Ok(models) => {
            let default = provider.default_model().to_string();
            // Providers like OpenRouter can list hundreds of models, which
            // would otherwise blow past the terminal height and make the
            // prompt unusable. `max_rows` turns the list into a scrollable
            // area, and `filter_mode` lets the user type to narrow it down
            // instead of scrolling through everything by hand.
            let mut select = cliclack::select(prompt)
                .filter_mode()
                .max_rows(MODEL_SELECT_MAX_ROWS);
            for model in &models {
                select = select.item(model.clone(), model.clone(), "");
            }
            if models.contains(&default) {
                select = select.initial_value(default);
            }
            Ok(select.interact()?)
        }
        Err(err) => {
            cliclack::log::warning(format!(
                "{}: could not list models ({err}); type one instead",
                provider.label(),
            ))?;
            Ok(cliclack::input(prompt)
                .default_input(provider.default_model())
                .interact()?)
        }
    }
}

/// Model aliases the installed `claude` CLI currently understands, read out of
/// its own `--help` text rather than hardcoded here.
///
/// There is no `/models` endpoint (or equivalent CLI command) a Claude
/// Pro/Max subscription can be probed with — [`fetch_models`] needs an API
/// key this transport does not have. But the `--model` option's own help
/// text names a few current aliases as examples (Anthropic updates that text
/// as new ones ship — a preview alias like `fable` shows up there before it
/// would in any list this crate could maintain), which is the closest thing
/// to a live source this wizard can read. See
/// [`extract_claude_model_aliases`] for the parsing, split out so it can be
/// tested without running the CLI. An empty result (missing CLI, or help
/// text in a shape the parser does not recognise) tells the caller to fall
/// back to a free-text prompt instead of showing an empty menu.
///
/// The help text's examples are illustrative rather than exhaustive, and
/// Anthropic has trimmed `haiku` from them even though it remains a working
/// alias (`claude --model haiku` still resolves) — it is stitched back in
/// here so the cheapest tier stays selectable without waiting on Anthropic
/// to re-add it to the example list.
fn claude_cli_model_aliases() -> Vec<String> {
    let output = std::process::Command::new(crate::agent::claude_cli::PROGRAM)
        .arg("--help")
        .output();
    let aliases = match output {
        Ok(output) if output.status.success() => {
            extract_claude_model_aliases(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    };
    with_haiku_restored(aliases)
}

/// Appends `haiku` to a non-empty alias list if the help text's parse did
/// not already surface it. Split out from [`claude_cli_model_aliases`] so
/// this can be tested without shelling out to the real CLI.
fn with_haiku_restored(mut aliases: Vec<String>) -> Vec<String> {
    if !aliases.is_empty() && !aliases.iter().any(|alias| alias == "haiku") {
        aliases.push("haiku".to_string());
    }
    aliases
}

/// Pulls the alias examples out of `claude --help`'s `--model` option
/// description, e.g.:
///
/// ```text
/// --model <model>   Model for the current session. Provide an alias for the
///                   latest model (e.g. 'fable', 'opus', or 'sonnet') or a
///                   model's full name (e.g. 'claude-fable-5').
/// ```
///
/// Only the *first* `(e.g. ...)` parenthetical is read: the second one is a
/// full model name example, not an alias, and including it would put a
/// non-alias entry in the menu. Any help text that does not match this shape
/// (a different CLI version, or none installed) yields an empty `Vec`.
fn extract_claude_model_aliases(help_text: &str) -> Vec<String> {
    let Some(option_start) = help_text.find("--model <model>") else {
        return Vec::new();
    };
    let rest = &help_text[option_start..];
    // Bounded to this option's own description — the next option's line
    // (two-space-indented `-`) or a generous cap, whichever comes first — so
    // a later, unrelated parenthetical is never mistaken for this one's.
    let window_end = rest[1..].find("\n  -").map_or(rest.len(), |i| i + 1);
    let window = &rest[..window_end.min(rest.len())];

    let Some(paren_start) = window.find("(e.g.") else {
        return Vec::new();
    };
    let Some(paren_len) = window[paren_start..].find(')') else {
        return Vec::new();
    };
    let parenthetical = &window[paren_start..paren_start + paren_len];

    parenthetical
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(|alias| alias.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|alias| !alias.is_empty())
        .collect()
}

/// Ask which model backs one *subscription*-authenticated role's route.
///
/// A subscription CLI's model is generally not knowable from here (which
/// models a ChatGPT plan allows cannot be probed at all — see
/// [`KnownProvider::subscription_model`]), so most providers still resolve to
/// that fixed value with no prompt. `claude` is the exception:
/// [`claude_cli_model_aliases`] can read the CLI's currently supported
/// aliases straight from its own help text, so a real choice is offered
/// there instead, with a "custom" option for anything not in that list (the
/// help text's examples are illustrative, not exhaustive) and the same
/// free-text fallback as [`choose_model`] if the CLI could not be read.
fn choose_subscription_model(role: AgentRole, provider: KnownProvider) -> Result<String> {
    if provider.subscription_transport() != Some(crate::config::Transport::ClaudeCli) {
        return Ok(provider.subscription_model().to_string());
    }

    let prompt = format!(
        "Model for the `{}` role (via {}, subscription)",
        role.label(),
        provider.label()
    );
    let aliases = claude_cli_model_aliases();
    if aliases.is_empty() {
        cliclack::log::warning(format!(
            "{}: could not read model aliases from `claude --help`; type one instead",
            provider.label(),
        ))?;
        return Ok(cliclack::input(prompt)
            .default_input(provider.subscription_model())
            .interact()?);
    }

    const CUSTOM: &str = "\0custom";
    let mut select = cliclack::select(prompt);
    for alias in &aliases {
        select = select.item(alias.clone(), alias.clone(), "");
    }
    select = select.item(CUSTOM.to_string(), "Custom (type a model id)", "");
    if aliases.iter().any(|alias| alias == "sonnet") {
        select = select.initial_value("sonnet".to_string());
    }
    let choice = select.interact()?;
    if choice == CUSTOM {
        Ok(cliclack::input(format!(
            "Model for the `{}` role (via {})",
            role.label(),
            provider.label()
        ))
        .default_input(provider.subscription_model())
        .interact()?)
    } else {
        Ok(choice)
    }
}

pub async fn run() -> Result<()> {
    let config_path = paths::config_file();
    cliclack::intro("llm-gateway init")?;

    if config_path.exists() {
        cliclack::log::warning(format!(
            "a config already exists at {}",
            config_path.display()
        ))?;
        let regenerate = cliclack::confirm(
            "regenerate it? every provider, route and key currently in the file will be replaced",
        )
        .initial_value(false)
        .interact()?;
        if !regenerate {
            cliclack::outro("kept the existing config.json — edit it directly instead")?;
            return Ok(());
        }

        // A wrong "yes" should still be recoverable — back up what was there
        // before anything gets overwritten.
        let backup_path = backup_path_for(&config_path);
        std::fs::copy(&config_path, &backup_path)?;
        cliclack::log::info(format!(
            "backed up the previous config to {}",
            backup_path.display()
        ))?;
    }

    // Not asked: every client the wizard knows about is supported
    // unconditionally, and `launch` lets a user switch between them at any
    // time — there is nothing a selection here would change.
    let clients: Vec<Client> = Client::ALL.to_vec();

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

    // Routes are scaffolded per functional role rather than per provider now
    // (see `AgentRole`) — `role-anthropic` or `role-openrouter` says nothing
    // about what a request needs to be doing to reach it; `role-architect` or
    // `role-reviewer` is the actual classification distinction. Skipped
    // entirely when no provider was selected — there is nothing to assign a
    // role to.
    let mut roles: Vec<(AgentRole, KnownProvider, String)> = Vec::new();
    if !selected_providers.is_empty() {
        let mut role_select =
            cliclack::multiselect("Which roles do you want to configure routes for?");
        for role in AgentRole::ALL {
            role_select = role_select.item(role, role.label(), role.description());
        }
        let selected_roles: Vec<AgentRole> = role_select
            .initial_values(AgentRole::ALL.to_vec())
            .interact()?;

        for role in selected_roles {
            let mut role_provider_select = cliclack::select(format!(
                "Which provider handles the `{}` role?",
                role.label()
            ));
            for provider in &selected_providers {
                let hint = if subscriptions.contains(provider) {
                    "subscription"
                } else {
                    ""
                };
                role_provider_select = role_provider_select.item(*provider, provider.label(), hint);
            }
            let provider = role_provider_select.interact()?;

            // A subscription CLI's model is not knowable from an API — there
            // is no key to probe `/models` with — so this branch cannot
            // reuse `choose_model`. `claude` is still asked for real, though:
            // see `choose_subscription_model`.
            let model = if subscriptions.contains(&provider) {
                choose_subscription_model(role, provider)?
            } else {
                let key = resolve_key_for_fetch(provider, &providers, &discovered);
                choose_model(role, provider, key.as_deref()).await?
            };
            roles.push((role, provider, model));
        }
    }

    let mut config = build_config_with_auth(&clients, &providers, storage, &subscriptions, &roles);
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
        let role_names: Vec<&str> = roles
            .iter()
            .filter(|(_, p, _)| p == provider)
            .map(|(role, _, _)| role.label())
            .collect();
        let via = if role_names.is_empty() {
            "no role assigned to it".to_string()
        } else {
            format!("via {}", role_names.join(", "))
        };
        cliclack::log::info(format!(
            "{}: runs on your subscription {via}, driven by `{}` — no key needed",
            provider.label(),
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

    let launch_examples = Client::ALL
        .iter()
        .map(|client| format!("  llm-gateway launch {}", client.launch_name()))
        .collect::<Vec<_>>()
        .join("\n");
    cliclack::outro(format!(
        "wrote {}\n\nnext steps:\n  llm-gateway config check\n  llm-gateway serve\n{launch_examples}\n\n\
         switch clients whenever you like — none of them need a config change.",
        config_path.display(),
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
    /// Every client the wizard knows about, in the order `launch` was
    /// documented for — all three are always supported, regardless of what
    /// the user actually runs.
    pub const ALL: [Client; 3] = [Self::Claude, Self::Codex, Self::Opencode];

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

    /// A model reference the subscription CLI accepts, for the scaffolded route.
    pub fn subscription_model(self) -> &'static str {
        match self {
            // The full id, not the `sonnet` alias: `sonnet-5` (which reads as
            // the natural next step from `sonnet`) is not a valid alias and
            // resolves to `model_not_found`, so the exact id is spelled out to
            // avoid that trap.
            Self::Anthropic => "claude-sonnet-5",
            // Codex has no alias, and which models a ChatGPT plan allows is
            // not knowable here — `default` means "whatever the CLI is
            // configured to use".
            _ => "default",
        }
    }

    /// A concrete `<model>` suggested as the starting point for this
    /// provider's classifiable route.
    ///
    /// Routing is decided purely by content classification now (see
    /// `crate::semantic::index`), so a route's model can no longer be a `*`
    /// wildcard resolved from the client's request — every route needs an
    /// explicit model. These are reasonable, current defaults, not a
    /// guarantee: the wizard prompts with this pre-filled and lets the user
    /// type a different one before it is written.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-5",
            Self::OpenAi => "gpt-5.1",
            Self::OpenRouter => "openai/gpt-5.1",
            Self::GithubCopilot => "gpt-5.4",
            Self::Gemini => "gemini-2.5-pro",
            Self::Xai => "grok-4",
            Self::Mistral => "mistral-large-latest",
            Self::DeepSeek => "deepseek-chat",
            Self::Groq => "llama-3.3-70b-versatile",
            Self::TogetherAi => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            Self::SakanaAi => "sakana-1",
            Self::Plamo => "plamo-2",
            Self::OllamaCloud => "qwen3:8b",
            Self::OllamaLocal => "qwen3:8b",
        }
    }
}

/// A functional role in a multi-agent workflow that a route is scaffolded
/// for.
///
/// Naming routes after the *provider* that serves them (`role-anthropic`,
/// `role-github-copilot`) reads as meaningless once more than one provider is
/// configured: the name says nothing about what the route is *for*. Naming
/// them after the job a request is doing — planning, exploring the codebase,
/// writing code, reviewing it — is what the classification corpus in
/// [`Self::description`] is actually distinguishing between, so that is what
/// the route name should say too. Each role is wired to exactly one provider
/// and model by the wizard; nothing stops a config from pointing several
/// roles at the same provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Manager,
    Architect,
    Explorer,
    WebResearcher,
    BrowserOperator,
    Implementer,
    Reviewer,
    Tester,
}

impl AgentRole {
    /// Every role the wizard offers, in menu order.
    pub const ALL: [AgentRole; 8] = [
        Self::Manager,
        Self::Architect,
        Self::Explorer,
        Self::WebResearcher,
        Self::BrowserOperator,
        Self::Implementer,
        Self::Reviewer,
        Self::Tester,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Manager => "manager",
            Self::Architect => "architect",
            Self::Explorer => "explorer",
            Self::WebResearcher => "web-researcher",
            Self::BrowserOperator => "browser-operator",
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Tester => "tester",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Manager => "Manager",
            Self::Architect => "Architect",
            Self::Explorer => "Explorer",
            Self::WebResearcher => "Web researcher",
            Self::BrowserOperator => "Browser operator",
            Self::Implementer => "Implementer",
            Self::Reviewer => "Reviewer",
            Self::Tester => "Tester",
        }
    }

    /// Classification text for this role's route.
    ///
    /// Every request is classified against every route's description (see
    /// `crate::semantic::index`), so this needs to describe the *kind of
    /// task* the role handles, not the role's name.
    pub fn description(self) -> &'static str {
        match self {
            Self::Manager => {
                "Coordinating a multi-step task: breaking work into subtasks, delegating to \
                 other roles, and tracking overall progress."
            }
            Self::Architect => {
                "Designing system architecture, planning approaches, and making structural or \
                 technical design decisions before code is written."
            }
            Self::Explorer => {
                "Exploring and reading an existing codebase to answer questions about how it is \
                 structured or where something is implemented, without editing it."
            }
            Self::WebResearcher => {
                "Researching information on the web: searching for documentation, current \
                 events, or facts outside the local codebase."
            }
            Self::BrowserOperator => {
                "Operating a web browser to interact with pages: clicking, filling forms, and \
                 navigating sites on the user's behalf."
            }
            Self::Implementer => "Writing or editing code to implement a feature or fix a bug.",
            Self::Reviewer => {
                "Reviewing a diff or pull request for bugs, security issues, and logic errors."
            }
            Self::Tester => "Writing or running tests, and diagnosing test failures.",
        }
    }

    /// Route name this role is scaffolded under.
    pub fn route_name(self) -> String {
        format!("role-{}", self.id())
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
///
/// `roles` is the wizard's role assignment: which [`AgentRole`] each route
/// represents, which provider serves it, and which model — already resolved,
/// e.g. from the provider's model list fetched over its API, or from
/// [`KnownProvider::subscription_model`] for a subscription-backed role.
/// Routes are named after the *role*, not the provider: `role-anthropic` and
/// `role-github-copilot` say nothing about what a request needs to be doing
/// to match, while `role-architect` or `role-reviewer` is exactly the
/// classification corpus in [`AgentRole::description`].
pub fn build_config(
    clients: &[Client],
    providers: &[(KnownProvider, Option<String>)],
    storage: KeyStorage,
    roles: &[(AgentRole, KnownProvider, String)],
) -> crate::config::Config {
    build_config_with_auth(clients, providers, storage, &[], roles)
}

/// [`build_config`], plus the providers whose credential is a subscription
/// rather than a key.
///
/// A subscription choice replaces the API-key provider entirely: it adds a
/// *different* provider id (`<id>-subscription`) with an agent-CLI transport,
/// and skips the plain API-key provider for that same provider — there is no
/// key to hold for it, so a provider entry for one would always fail with an
/// empty credential. A role assigned to that provider resolves its route to
/// the `<id>-subscription` provider automatically.
pub fn build_config_with_auth(
    _clients: &[Client],
    providers: &[(KnownProvider, Option<String>)],
    storage: KeyStorage,
    subscriptions: &[KnownProvider],
    roles: &[(AgentRole, KnownProvider, String)],
) -> crate::config::Config {
    use crate::config::{ApiKind, Config, ModelConfig, ProviderConfig, RouteConfig};

    let selected: Vec<KnownProvider> = providers.iter().map(|(p, _)| *p).collect();
    let has = |p: KnownProvider| selected.contains(&p);
    let is_subscription_only = |p: KnownProvider| subscriptions.contains(&p);
    let literal_for = |p: KnownProvider| {
        providers
            .iter()
            .find(|(candidate, _)| *candidate == p)
            .and_then(|(_, literal)| literal.clone())
    };

    let mut config = Config::default();

    for (provider, literal) in providers {
        if is_subscription_only(*provider) {
            continue;
        }
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
    // `openrouter/anthropic/<model>`; expose it under its own id so a route
    // can reach an OpenRouter-hosted model over the Anthropic protocol
    // without crossing `ApiKind`s.
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

    // Routes are named after the *role* a request is doing, not the provider
    // serving it — see `AgentRole`. Each assignment resolves to whichever
    // provider id actually holds the credential: the subscription id when the
    // role's provider was chosen as a subscription, its plain id otherwise.
    let target_for = |provider: KnownProvider| -> String {
        if is_subscription_only(provider) {
            provider.subscription_id()
        } else {
            provider.id().to_string()
        }
    };
    for (role, provider, model) in roles {
        config.routes.insert(
            role.route_name(),
            RouteConfig {
                description: Some(crate::config::Description(role.description().to_string())),
                model: ModelConfig {
                    default: format!("{}/{model}", target_for(*provider)),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    // The reserved catch-all for whatever does not clear the classification
    // threshold on any other route. Points at the first role assignment when
    // one exists; otherwise falls back to the first selected provider that
    // actually has a provider entry — a subscription-only provider has none,
    // so it is skipped in favor of one that does; if every selected provider
    // is subscription-only, the first subscription's own id and model are
    // used instead, since that is the only entry that exists.
    let default_target = roles
        .first()
        .map(|(_, provider, model)| format!("{}/{model}", target_for(*provider)))
        .or_else(|| {
            providers
                .iter()
                .map(|(p, _)| *p)
                .find(|p| !is_subscription_only(*p))
                .map(|p| format!("{}/{}", p.id(), p.default_model()))
        })
        .or_else(|| {
            subscriptions
                .first()
                .map(|p| format!("{}/{}", p.subscription_id(), p.subscription_model()))
        });
    if let Some(default_model) = default_target {
        config.routes.insert(
            crate::config::DEFAULT_ROUTE.to_string(),
            RouteConfig {
                description: Some(crate::config::Description(
                    "Fallback for requests that do not clearly match any other route's \
                     description."
                        .to_string(),
                )),
                model: ModelConfig {
                    default: default_model,
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
    }

    // The subscription provider entry itself — not a route, since a role
    // assignment above is what actually points a route at it via
    // `target_for`. A subscription chosen for no role still gets a provider
    // entry (harmless; `validate` only warns about it), same as an API-key
    // provider chosen for no role.
    for provider in subscriptions {
        let Some(transport) = provider.subscription_transport() else {
            continue;
        };
        let Some(api) = transport.fixed_api() else {
            continue;
        };
        config.providers.insert(
            provider.subscription_id(),
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

    /// A realistic excerpt of `claude --help`'s `--model` option, wrapped the
    /// way a terminal actually wraps it — this is what
    /// `extract_claude_model_aliases` has to parse, not a tidy one-liner.
    const CLAUDE_HELP_MODEL_EXCERPT: &str = "\
  --mcp-config <configs...>             Load MCP servers from JSON files or
                                        strings (space-separated)
  --model <model>                       Model for the current session. Provide
                                        an alias for the latest model (e.g.
                                        'fable', 'opus', or 'sonnet') or a
                                        model's full name (e.g.
                                        'claude-fable-5').
  -n, --name <name>                     Set a display name for this session
";

    #[test]
    fn extracts_aliases_from_the_models_own_help_text() {
        let aliases = extract_claude_model_aliases(CLAUDE_HELP_MODEL_EXCERPT);
        assert_eq!(aliases, vec!["fable", "opus", "sonnet"]);
    }

    /// `extract_claude_model_aliases` only reflects what `--help` prints, so
    /// `haiku` reappearing is exercised on [`with_haiku_restored`] instead —
    /// this test just documents that the raw parse still omits it, the gap
    /// the wrapper fills in.
    #[test]
    fn the_raw_help_text_parse_no_longer_lists_haiku() {
        let aliases = extract_claude_model_aliases(CLAUDE_HELP_MODEL_EXCERPT);
        assert!(!aliases.iter().any(|alias| alias == "haiku"));
    }

    #[test]
    fn restores_haiku_when_the_help_text_omitted_it() {
        let aliases = with_haiku_restored(vec!["fable".to_string(), "opus".to_string()]);
        assert_eq!(aliases, vec!["fable", "opus", "haiku"]);
    }

    #[test]
    fn does_not_duplicate_haiku_if_help_text_already_lists_it() {
        let aliases = with_haiku_restored(vec!["opus".to_string(), "haiku".to_string()]);
        assert_eq!(aliases, vec!["opus", "haiku"]);
    }

    #[test]
    fn leaves_an_empty_alias_list_empty() {
        assert!(with_haiku_restored(Vec::new()).is_empty());
    }

    #[test]
    fn does_not_pick_up_the_full_name_examples_second_parenthetical() {
        let aliases = extract_claude_model_aliases(CLAUDE_HELP_MODEL_EXCERPT);
        assert!(!aliases.iter().any(|alias| alias == "claude-fable-5"));
    }

    #[test]
    fn a_single_alias_still_parses_without_an_or_clause() {
        let help = "--model <model>  Model for the session (e.g. 'opus').\n  -n, --name";
        assert_eq!(extract_claude_model_aliases(help), vec!["opus"]);
    }

    #[test]
    fn unrecognised_help_text_yields_no_aliases_rather_than_panicking() {
        assert!(extract_claude_model_aliases("").is_empty());
        assert!(extract_claude_model_aliases("--model <model> no parens here").is_empty());
    }

    /// The scaffolded Copilot provider must be reachable from Claude Code,
    /// which means `openai-chat` plus the translation layer — the whole reason
    /// it can be offered at all.
    #[test]
    fn a_subscription_choice_scaffolds_a_cli_transport_and_a_role_route() {
        let config = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[KnownProvider::Anthropic],
            &[(
                AgentRole::Manager,
                KnownProvider::Anthropic,
                "claude-sonnet-5".to_string(),
            )],
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

        let route = config.routes.get("role-manager").expect("route");
        assert_eq!(
            route.model.default,
            "anthropic-subscription/claude-sonnet-5"
        );
        assert!(route.description.is_some(), "the route explains its limits");
    }

    /// The plain API-key provider is skipped when Subscription is chosen for
    /// that same provider: there is no key to hold for it, so a role assigned
    /// to it resolves to the subscription id instead of an always-failing
    /// empty-credential provider.
    #[test]
    fn choosing_a_subscription_does_not_also_scaffold_the_api_key_provider() {
        let config = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[KnownProvider::Anthropic],
            &[(
                AgentRole::Manager,
                KnownProvider::Anthropic,
                "claude-sonnet-5".to_string(),
            )],
        );
        assert!(!config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("anthropic-subscription"));
        assert_eq!(
            config.routes["role-manager"].model.default,
            "anthropic-subscription/claude-sonnet-5"
        );
    }

    /// Choosing subscriptions for both Anthropic and OpenAI used to collide:
    /// both wrote to the fixed route name `role-subscription`, so the second
    /// silently overwrote the first. Routes are now named after the role
    /// assigned to each, so two different roles each get their own route.
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
            &[
                (
                    AgentRole::Manager,
                    KnownProvider::Anthropic,
                    "claude-sonnet-5".to_string(),
                ),
                (
                    AgentRole::Implementer,
                    KnownProvider::OpenAi,
                    "default".to_string(),
                ),
            ],
        );
        assert_eq!(
            config.routes["role-manager"].model.default,
            "anthropic-subscription/claude-sonnet-5"
        );
        assert_eq!(
            config.routes["role-implementer"].model.default,
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
            &[],
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
            &[(
                AgentRole::Implementer,
                KnownProvider::OpenAi,
                "default".to_string(),
            )],
        );
        let provider = &config.providers["openai-subscription"];
        assert_eq!(provider.transport, crate::config::Transport::CodexCli);
        // Codex renders as openai-chat, which is what the most clients reach.
        assert_eq!(provider.api, crate::config::ApiKind::OpenaiChat);
        assert_eq!(
            config.routes["role-implementer"].model.default,
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
        let roles = [(
            AgentRole::Manager,
            KnownProvider::Anthropic,
            "claude-sonnet-5".to_string(),
        )];
        let with = build_config_with_auth(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &[],
            &roles,
        );
        let without = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, None)],
            KeyStorage::Keychain,
            &roles,
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
            &[],
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
            &[],
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

    /// OpenRouter no longer auto-wires itself as a fallback on any route — an
    /// auto-guessed cross-provider fallback used the same `*` wildcard
    /// mechanism this config no longer supports, and a fallback the user did
    /// not choose is better left for them to add explicitly. The
    /// `openrouter-anthropic` provider entry itself is still scaffolded,
    /// ready for a route to reference explicitly.
    #[test]
    fn openrouter_anthropic_provider_exists_but_is_not_auto_wired_as_a_fallback() {
        let config = build_config(
            &[Client::Claude],
            &[
                (KnownProvider::Anthropic, Some("sk-ant-test".to_string())),
                (KnownProvider::OpenRouter, Some("sk-or-test".to_string())),
            ],
            KeyStorage::Literal,
            &[(
                AgentRole::Architect,
                KnownProvider::Anthropic,
                "claude-sonnet-5".to_string(),
            )],
        );

        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("openrouter-anthropic"));

        let route = config
            .routes
            .get("role-architect")
            .expect("role-architect route");
        assert_eq!(route.model.default, "anthropic/claude-sonnet-5");
        assert!(route.model.fallbacks.is_empty());

        assert!(!config.routes.contains_key("role-implementer"));
    }

    #[test]
    fn codex_only_with_openai_has_no_fallback_without_openrouter() {
        let config = build_config(
            &[Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Literal,
            &[(
                AgentRole::Implementer,
                KnownProvider::OpenAi,
                "gpt-5.1".to_string(),
            )],
        );

        let route = config
            .routes
            .get("role-implementer")
            .expect("role-implementer route");
        assert_eq!(route.model.default, "openai/gpt-5.1");
        assert!(route.model.fallbacks.is_empty());

        assert!(!config.routes.contains_key("role-architect"));
        assert!(!config.providers.contains_key("openrouter-anthropic"));
    }

    #[test]
    fn a_role_not_assigned_gets_no_route() {
        let config = build_config(
            &[Client::Claude, Client::Codex],
            &[(KnownProvider::OpenAi, Some("sk-test".to_string()))],
            KeyStorage::Env,
            &[(
                AgentRole::Implementer,
                KnownProvider::OpenAi,
                "gpt-5.1".to_string(),
            )],
        );

        assert!(!config.routes.contains_key("role-architect"));
        assert!(config.routes.contains_key("role-implementer"));
    }

    #[test]
    fn a_default_route_is_always_scaffolded_when_a_provider_is_selected() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, Some("sk-ant-test".to_string()))],
            KeyStorage::Literal,
            &[],
        );

        let route = config
            .routes
            .get(crate::config::DEFAULT_ROUTE)
            .expect("default route");
        assert_eq!(route.model.default, "anthropic/claude-sonnet-5");
        assert!(route.description.is_some());
    }

    /// When at least one role is assigned, the reserved `default` route
    /// should point at the same target as the first one — not silently fall
    /// back to a provider's built-in default model, which may not be the one
    /// the wizard's user actually picked.
    #[test]
    fn the_default_route_prefers_the_first_role_assignment() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, Some("sk-ant-test".to_string()))],
            KeyStorage::Literal,
            &[(
                AgentRole::Architect,
                KnownProvider::Anthropic,
                "claude-opus-9".to_string(),
            )],
        );

        let route = config
            .routes
            .get(crate::config::DEFAULT_ROUTE)
            .expect("default route");
        assert_eq!(route.model.default, "anthropic/claude-opus-9");
    }

    #[test]
    fn every_assigned_role_gets_a_description_for_classification() {
        let config = build_config(
            &[Client::Claude],
            &[
                (KnownProvider::Anthropic, Some("sk-ant-test".to_string())),
                (KnownProvider::OpenRouter, Some("sk-or-test".to_string())),
            ],
            KeyStorage::Literal,
            &[
                (
                    AgentRole::Architect,
                    KnownProvider::Anthropic,
                    "claude-sonnet-5".to_string(),
                ),
                (
                    AgentRole::Explorer,
                    KnownProvider::OpenRouter,
                    "openai/gpt-5.1".to_string(),
                ),
            ],
        );

        for name in ["role-architect", "role-explorer"] {
            let route = config.routes.get(name).unwrap_or_else(|| panic!("{name}"));
            assert!(
                route.description.is_some(),
                "{name} needs a description to be a classification candidate"
            );
        }
    }

    /// Every role has a unique, kebab-case id and a route name derived from
    /// it — the whole point of naming routes by role instead of provider.
    #[test]
    fn every_role_has_a_unique_id_and_route_name() {
        let mut ids: Vec<&str> = AgentRole::ALL.iter().map(|r| r.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), AgentRole::ALL.len(), "role ids must be unique");

        for role in AgentRole::ALL {
            assert_eq!(role.route_name(), format!("role-{}", role.id()));
            assert!(!role.description().is_empty());
        }
    }

    #[test]
    fn literal_storage_uses_the_given_value() {
        let config = build_config(
            &[Client::Claude],
            &[(KnownProvider::Anthropic, Some("sk-ant-abc".to_string()))],
            KeyStorage::Literal,
            &[],
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
            &[],
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
            &[],
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
            &[],
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
