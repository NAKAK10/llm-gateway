//! `llm-gateway init` — write a first config.
//!
//! Asks three questions (main client, providers, how to store keys), writes a
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

use crate::error::Result;

/// Permissions for `config.json`. It can hold API keys in the clear.
pub const CONFIG_MODE: u32 = 0o600;

pub fn run() -> Result<()> {
    todo!("src/cli/init.rs")
}

/// Which client the user reaches for most. Decides which routes and `launch`
/// entry the generated config starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryClient {
    Claude,
    Codex,
    Both,
}

/// A provider the wizard knows how to scaffold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProvider {
    Anthropic,
    OpenAi,
    OpenRouter,
    OllamaCloud,
    OllamaLocal,
}

impl KnownProvider {
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::OllamaCloud => "ollama-cloud",
            Self::OllamaLocal => "ollama-local",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::OllamaCloud => "Ollama Cloud",
            Self::OllamaLocal => "Ollama (local)",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::OllamaCloud => "https://ollama.com/v1",
            Self::OllamaLocal => "http://127.0.0.1:11434/v1",
        }
    }

    pub fn api(self) -> crate::config::ApiKind {
        use crate::config::ApiKind;
        match self {
            Self::Anthropic => ApiKind::AnthropicMessages,
            Self::OpenAi => ApiKind::OpenaiResponses,
            Self::OpenRouter | Self::OllamaCloud | Self::OllamaLocal => ApiKind::OpenaiChat,
        }
    }

    /// Environment variable name used by the `${VAR}` option.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
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
    primary: PrimaryClient,
    providers: &[(KnownProvider, Option<String>)],
    storage: KeyStorage,
) -> crate::config::Config {
    let _ = (primary, providers, storage);
    todo!("src/cli/init.rs")
}
