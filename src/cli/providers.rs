//! `llm-gateway providers` — is each upstream actually reachable?
//!
//! One probe per configured provider, in parallel, with a per-provider verdict:
//! resolved key or not, connection or not, and the HTTP status of a cheap
//! request. Exists so "the gateway is broken" can be split into "your key is
//! wrong" versus "the provider is down" without reading any logs.

use std::time::{Duration, Instant};

use clap::Args;
use futures_util::future::join_all;

use crate::cli::init::KnownProvider;
use crate::config::{ApiKind, Config, ProviderConfig, SecretRef, Transport};
use crate::error::{Error, Result};
use crate::paths;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Probe result for one provider.
#[derive(Debug)]
pub struct Probe {
    pub id: String,
    pub base_url: String,
    /// Whether the API key reference resolved (not whether it is *valid* —
    /// that is what the HTTP status is for).
    pub key_resolved: bool,
    /// HTTP status of the probe request, if a response came back at all.
    pub status: Option<u16>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let client = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build()?;

    let probes = join_all(
        config
            .providers
            .iter()
            .map(|(id, provider)| probe(&client, id.clone(), provider.clone())),
    )
    .await;

    let mut table = comfy_table::Table::new();
    table.set_header(vec!["provider", "api", "key", "status", "time"]);

    let mut unreachable = 0usize;
    for (probe, api) in &probes {
        let ok = probe
            .status
            .map(|code| (200..300).contains(&code))
            .unwrap_or(false);
        if !(probe.key_resolved && ok) {
            unreachable += 1;
        }

        let status = match (probe.status, &probe.error) {
            (Some(code), _) => code.to_string(),
            (None, Some(err)) => format!("{} ({err})", probe.base_url),
            (None, None) => format!("{} (unreachable)", probe.base_url),
        };

        table.add_row(vec![
            probe.id.clone(),
            api.as_str().to_string(),
            if probe.key_resolved {
                "resolved".to_string()
            } else {
                "unresolved".to_string()
            },
            status,
            format!("{}ms", probe.elapsed_ms),
        ]);
    }

    println!("{table}");

    if unreachable > 0 {
        return Err(Error::Other(format!(
            "{unreachable} provider(s) unreachable"
        )));
    }

    Ok(())
}

/// Resolve the key, then make one cheap request to see if the provider
/// answers at all. Errors at either step are recorded on the [`Probe`] rather
/// than aborting — one bad provider should not hide the rest of the table.
async fn probe(client: &reqwest::Client, id: String, provider: ProviderConfig) -> (Probe, ApiKind) {
    let start = Instant::now();

    // A `claude-cli` provider has nothing to connect to. The equivalent question
    // is "is the binary there?", and answering it here keeps the table's
    // promise: every configured provider gets a verdict.
    if provider.transport.is_agent_cli() {
        let available = crate::agent::is_available_for(provider.transport);
        return (
            Probe {
                id,
                base_url: format!("{} (local process)", agent_program(provider.transport)),
                // The CLI holds its own login; there is no reference for us to
                // resolve, so "resolved" means "the tool is installed".
                key_resolved: available,
                status: available.then_some(200),
                error: (!available).then(|| {
                    format!(
                        "`{}` not found on PATH — install it and log in once",
                        agent_program(provider.transport)
                    )
                }),
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
            provider.api,
        );
    }

    let (key, key_resolved) = match &provider.api_key {
        Some(secret) => match secret.resolve() {
            Ok(value) => (Some(value), true),
            Err(_) => (None, false),
        },
        // Nothing to resolve — e.g. a local endpoint's literal `"local"`
        // placeholder never needs a header at all.
        None => (None, true),
    };

    let request = if provider.api == ApiKind::AnthropicMessages {
        let mut req = client.get(format!("{}/v1/models", provider.base_url));
        if let Some(key) = &key {
            req = req.header("x-api-key", key);
        }
        req.header("anthropic-version", "2023-06-01")
    } else {
        let mut req = client.get(format!("{}/models", provider.base_url));
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }
        req
    };

    // The provider's own headers go on the probe too, or a provider that needs
    // one would be reported as broken when it is merely configured — and this
    // command exists precisely to tell those two apart.
    let mut request = request;
    for (name, value) in &provider.headers {
        request = request.header(name, value);
    }

    let (status, error) = match request.send().await {
        Ok(response) => (Some(response.status().as_u16()), None),
        Err(err) => (None, Some(err.to_string())),
    };

    let probe = Probe {
        id,
        base_url: provider.base_url.clone(),
        key_resolved,
        status,
        error,
        elapsed_ms: start.elapsed().as_millis() as u64,
    };

    (probe, provider.api)
}

/// The binary an agent transport runs, for the probe's messages.
fn agent_program(transport: crate::config::Transport) -> &'static str {
    match transport {
        crate::config::Transport::CodexCli => crate::agent::codex_cli::PROGRAM,
        _ => crate::agent::claude_cli::PROGRAM,
    }
}

/// `llm-gateway providers add`.
///
/// Every field is optional on the command line: anything not given (and not
/// implied by `--preset`) is asked for interactively, so this works equally
/// well as `providers add --preset ollama-local` (silent, no key needed) and
/// as a bare `providers add` (a guided menu of every known preset, same list
/// `init` offers, plus a "custom" option for anything else).
#[derive(Args)]
pub struct AddArgs {
    /// Id to store this provider under in `config.json` — what a route's
    /// `model` refers to as `<id>/<model>`. Defaults to the preset's own id.
    #[arg(long)]
    pub id: Option<String>,

    /// A provider this wizard already knows how to scaffold. One of:
    /// anthropic, openai, openrouter, github-copilot, gemini, xai, mistral,
    /// deepseek, groq, together, sakana, plamo, ollama-cloud, ollama-local.
    /// Omit for a fully custom HTTP provider.
    #[arg(long)]
    pub preset: Option<String>,

    /// Base URL, e.g. `http://127.0.0.1:11434/v1`. Required for a custom
    /// provider (no `--preset`); overrides the preset's own default.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Wire protocol: `openai-chat`, `openai-responses`, or
    /// `anthropic-messages`. Defaults to the preset's own protocol, or is
    /// asked for interactively when there is no preset to default from.
    #[arg(long)]
    pub api: Option<String>,

    /// Literal API key, written into config.json in the clear.
    #[arg(long, conflicts_with_all = ["key_env", "key_command"])]
    pub key: Option<String>,

    /// Read the key from this environment variable at request time.
    #[arg(long, conflicts_with_all = ["key", "key_command"])]
    pub key_env: Option<String>,

    /// Run this shell command to obtain the key at request time (e.g. `"gh
    /// auth token"`).
    #[arg(long, conflicts_with_all = ["key", "key_env"])]
    pub key_command: Option<String>,
}

pub fn add(args: AddArgs) -> Result<()> {
    let config_path = paths::config_file();
    let mut config = crate::cli::config_write::read_or_default(&config_path)?;

    let preset = match &args.preset {
        Some(raw) => Some(parse_preset(raw)?),
        None if args.base_url.is_none() => Some(prompt_preset()?),
        None => None,
    };

    let id = match &args.id {
        Some(id) => id.clone(),
        None => match preset {
            Some(p) => p.id().to_string(),
            None => cliclack::input("Provider id (used as `<id>/<model>` in routes)").interact()?,
        },
    };

    if config.providers.contains_key(&id) {
        return Err(Error::Other(format!(
            "provider `{id}` already exists in {} — edit it there directly, there is no \
             `providers edit` yet",
            config_path.display()
        )));
    }

    let base_url = match &args.base_url {
        Some(base_url) => base_url.clone(),
        None => match preset {
            Some(p) => p.base_url().to_string(),
            None => cliclack::input("Base URL").interact()?,
        },
    };

    let api = match &args.api {
        Some(raw) => parse_api_kind(raw)?,
        None => match preset {
            Some(p) => p.api(),
            None => prompt_api_kind()?,
        },
    };

    let needs_key = preset.map(|p| p.needs_key()).unwrap_or(true);
    let api_key = resolve_key(&args, preset, needs_key)?;

    let headers = preset
        .map(|p| {
            p.headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();

    config.providers.insert(
        id.clone(),
        ProviderConfig {
            base_url,
            api,
            transport: Transport::Http,
            agent_args: Vec::new(),
            api_key,
            headers,
            inject_usage: true,
            timeout_seconds: None,
            max_concurrent: None,
        },
    );

    crate::cli::config_write::write_config(&config, &config_path)?;
    cliclack::log::success(format!(
        "added provider `{id}` to {} — point a route's `model` at `{id}/<model-name>`, \
         or run `llm-gateway route add` now",
        config_path.display()
    ))?;
    Ok(())
}

fn parse_preset(raw: &str) -> Result<KnownProvider> {
    KnownProvider::ALL
        .into_iter()
        .find(|p| p.id() == raw)
        .ok_or_else(|| {
            let known = KnownProvider::ALL
                .iter()
                .map(|p| p.id())
                .collect::<Vec<_>>()
                .join(", ");
            Error::Other(format!("unknown preset `{raw}` — one of: {known}"))
        })
}

fn prompt_preset() -> Result<KnownProvider> {
    let mut select = cliclack::select("Provider").filter_mode();
    for p in KnownProvider::ALL {
        select = select.item(p, format!("{} ({})", p.label(), p.id()), p.base_url());
    }
    Ok(select.interact()?)
}

fn parse_api_kind(raw: &str) -> Result<ApiKind> {
    match raw {
        "openai-chat" => Ok(ApiKind::OpenaiChat),
        "openai-responses" => Ok(ApiKind::OpenaiResponses),
        "anthropic-messages" => Ok(ApiKind::AnthropicMessages),
        other => Err(Error::Other(format!(
            "unknown --api `{other}` — one of: openai-chat, openai-responses, anthropic-messages"
        ))),
    }
}

fn prompt_api_kind() -> Result<ApiKind> {
    Ok(cliclack::select("Wire protocol")
        .item(
            ApiKind::OpenaiChat,
            "openai-chat",
            "POST {base_url}/chat/completions",
        )
        .item(
            ApiKind::OpenaiResponses,
            "openai-responses",
            "POST {base_url}/responses",
        )
        .item(
            ApiKind::AnthropicMessages,
            "anthropic-messages",
            "POST {base_url}/v1/messages",
        )
        .interact()?)
}

/// `None` means "no header at all" (`ProviderConfig::api_key` stays unset) —
/// only reachable for a local endpoint (`--preset ollama-local`, or a custom
/// provider that explicitly says it needs no key).
fn resolve_key(
    args: &AddArgs,
    preset: Option<KnownProvider>,
    needs_key: bool,
) -> Result<Option<SecretRef>> {
    if let Some(literal) = &args.key {
        return Ok(Some(SecretRef::new(literal.clone())));
    }
    if let Some(var) = &args.key_env {
        return Ok(Some(SecretRef::new(format!("${{{var}}}"))));
    }
    if let Some(command) = &args.key_command {
        return Ok(Some(SecretRef::new(format!("command:{command}"))));
    }
    if !needs_key {
        return Ok(None);
    }

    let default_var = preset.map(|p| p.env_var().to_string());
    let storage = cliclack::select("How should the key be stored?")
        .item(0u8, "Environment variable", "read at request time")
        .item(1u8, "Literal", "written into config.json in the clear")
        .item(2u8, "Command", "e.g. a CLI that already holds it")
        .interact()?;
    match storage {
        0 => {
            let mut prompt = cliclack::input("Environment variable name");
            if let Some(var) = &default_var {
                prompt = prompt.default_input(var);
            }
            let var: String = prompt.interact()?;
            Ok(Some(SecretRef::new(format!("${{{var}}}"))))
        }
        1 => {
            let key: String = cliclack::input("API key").interact()?;
            Ok(Some(SecretRef::new(key)))
        }
        _ => {
            let command: String = cliclack::input("Shell command").interact()?;
            Ok(Some(SecretRef::new(format!("command:{command}"))))
        }
    }
}

#[cfg(test)]
mod add_tests {
    use super::*;

    #[test]
    fn ollama_presets_resolve_by_id() {
        assert_eq!(
            parse_preset("ollama-local").unwrap(),
            KnownProvider::OllamaLocal
        );
        assert_eq!(
            parse_preset("ollama-cloud").unwrap(),
            KnownProvider::OllamaCloud
        );
    }

    #[test]
    fn every_known_provider_id_round_trips_through_parse_preset() {
        for provider in KnownProvider::ALL {
            assert_eq!(parse_preset(provider.id()).unwrap(), provider);
        }
    }

    #[test]
    fn an_unknown_preset_lists_the_valid_ones_in_its_error() {
        let err = parse_preset("made-up-thing").unwrap_err().to_string();
        assert!(err.contains("made-up-thing"));
        assert!(err.contains("ollama-local"));
        assert!(err.contains("ollama-cloud"));
    }

    #[test]
    fn api_kind_strings_round_trip() {
        assert_eq!(parse_api_kind("openai-chat").unwrap(), ApiKind::OpenaiChat);
        assert_eq!(
            parse_api_kind("openai-responses").unwrap(),
            ApiKind::OpenaiResponses
        );
        assert_eq!(
            parse_api_kind("anthropic-messages").unwrap(),
            ApiKind::AnthropicMessages
        );
    }

    #[test]
    fn an_unknown_api_kind_is_an_error() {
        assert!(parse_api_kind("carrier-pigeon").is_err());
    }

    /// `--key`/`--key-env`/`--key-command` must win over whatever a preset
    /// would otherwise ask for — resolving from the command line is the
    /// whole point of passing them, not just a hint.
    #[test]
    fn an_explicit_key_flag_skips_the_preset_default() {
        let args = AddArgs {
            id: None,
            preset: None,
            base_url: None,
            api: None,
            key: Some("sk-literal".to_string()),
            key_env: None,
            key_command: None,
        };
        let key = resolve_key(&args, Some(KnownProvider::OllamaCloud), true)
            .unwrap()
            .unwrap();
        assert_eq!(key.raw(), "sk-literal");
    }

    #[test]
    fn key_env_flag_produces_a_variable_reference() {
        let args = AddArgs {
            id: None,
            preset: None,
            base_url: None,
            api: None,
            key: None,
            key_env: Some("MY_VAR".to_string()),
            key_command: None,
        };
        let key = resolve_key(&args, None, true).unwrap().unwrap();
        assert_eq!(key.raw(), "${MY_VAR}");
    }

    #[test]
    fn key_command_flag_produces_a_command_reference() {
        let args = AddArgs {
            id: None,
            preset: None,
            base_url: None,
            api: None,
            key: None,
            key_env: None,
            key_command: Some("gh auth token".to_string()),
        };
        let key = resolve_key(&args, None, true).unwrap().unwrap();
        assert_eq!(key.raw(), "command:gh auth token");
    }

    /// A provider that needs no key at all (`ollama-local`) must resolve to
    /// no `apiKey` field rather than prompting — the whole reason
    /// `KnownProvider::needs_key` exists.
    #[test]
    fn a_provider_that_needs_no_key_resolves_to_none_without_prompting() {
        let args = AddArgs {
            id: None,
            preset: None,
            base_url: None,
            api: None,
            key: None,
            key_env: None,
            key_command: None,
        };
        let key = resolve_key(&args, Some(KnownProvider::OllamaLocal), false).unwrap();
        assert!(key.is_none());
    }
}
