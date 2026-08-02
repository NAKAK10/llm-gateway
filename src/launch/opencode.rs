//! `llm-gateway launch opencode`
//!
//! opencode merges configuration from several sources in a fixed order, and
//! `OPENCODE_CONFIG_CONTENT` is applied *after* a project's `opencode.json`.
//! `OPENCODE_CONFIG` is applied before it and therefore loses to any project
//! config — so the inline form is the only one that reliably wins.
//!
//! Two consequences worth knowing:
//!
//! - **`{env:VAR}` and `{file:...}` placeholders are not expanded inside
//!   `OPENCODE_CONFIG_CONTENT`** (anomalyco/opencode#13219). The token has to be
//!   embedded literally. Since this launcher generates the JSON, nobody has to
//!   hand-write a secret — but it does mean the value is in the child's
//!   environment, which is why it is redacted when printed.
//! - **`models` keys must match `GET /v1/models` exactly.** On a mismatch
//!   opencode offers no models and reports nothing. [`verify_models`] checks
//!   against the running gateway before the child starts, turning a silent
//!   failure into a message.
//!
//! Unlike Codex (global provider) and Claude Code (process-wide env), opencode
//! selects a provider *per model reference* — an agent file pinning
//! `model: openai/gpt-…` would go straight to OpenAI and silently bypass the
//! gateway. So the injected config also **redirects the built-in providers**
//! listed in `launch.opencode.overrideProviders` (default: see
//! `crate::config::default_opencode_overrides`) to the gateway. Per-agent
//! model choices keep working unchanged; the model id is forwarded as-is and
//! reaches the gateway on the matching endpoint, where routing is decided by
//! classification, not by the id itself.
//!
//! Redirecting closes the bypass only for providers *in* the list. An agent
//! or `opencode.json` that pins a provider outside it (`google`, or anything
//! a user added themselves) still goes straight to that provider with no
//! error — the same silent-bypass shape, just not caught by the redirect.
//! [`detect_pinned_bypasses`] scans for that read-only, the same way
//! `crate::launch::claude::detect_conflicts` scans `~/.claude/settings.json`.
//!
//! `--isolate` adds `--pure`, which disables external plugins.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{default_opencode_overrides, Config};
use crate::error::Result;
use crate::launch::Invocation;

pub fn build(
    config: &Config,
    model: &str,
    models: &[String],
    isolate: bool,
    auto_route: bool,
    args: &[String],
) -> Result<Invocation> {
    let base_url = config.server.base_url();
    let api_key = match &config.server.api_key {
        Some(key) => Some(key.resolve()?),
        None => None,
    };

    let wanted_models = resolved_models(config, models);
    let overrides: Vec<String> = config
        .launch
        .opencode
        .as_ref()
        .map(|c| c.override_providers.clone())
        .unwrap_or_else(default_opencode_overrides);
    let content = config_content(
        &base_url,
        api_key.as_deref(),
        &wanted_models,
        &overrides,
        auto_route,
    );

    let env = vec![(
        "OPENCODE_CONFIG_CONTENT".to_string(),
        serde_json::to_string(&content)?,
    )];

    let mut all_args = vec!["-m".to_string(), format!("gateway/{model}")];
    if isolate {
        all_args.push("--pure".to_string());
    }
    all_args.extend(args.iter().cloned());

    Ok(Invocation {
        program: "opencode".to_string(),
        args: all_args,
        env,
        warnings: detect_pinned_bypasses(&overrides),
    })
}

/// `models`, or every non-wildcard route when it is empty.
///
/// Shared between [`build`] and the caller's [`verify_models`] check so both
/// agree on the same list.
pub(crate) fn resolved_models(config: &Config, models: &[String]) -> Vec<String> {
    if models.is_empty() {
        config
            .listable_routes()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        models.to_vec()
    }
}

/// The inline config injected via `OPENCODE_CONFIG_CONTENT`.
pub fn config_content(
    base_url: &str,
    api_key: Option<&str>,
    models: &[String],
    override_providers: &[String],
    auto_route: bool,
) -> serde_json::Value {
    let mut model_entries = serde_json::Map::new();
    for m in models {
        model_entries.insert(m.clone(), serde_json::json!({}));
    }

    let mut providers = serde_json::Map::new();
    providers.insert(
        "gateway".to_string(),
        serde_json::json!({
            "npm": "@ai-sdk/openai-compatible",
            "options": gateway_options(base_url, api_key, auto_route),
            "models": model_entries,
        }),
    );

    // Redirect the named built-in providers so per-agent `model:
    // openai/…` references also flow through the gateway. Only `options`
    // is set — opencode merges configs key-by-key, so the provider keeps
    // its native npm package (and therefore its native wire protocol,
    // which the gateway speaks on the matching endpoint).
    for id in override_providers {
        providers.insert(
            id.clone(),
            serde_json::json!({ "options": gateway_options(base_url, api_key, auto_route) }),
        );
    }

    serde_json::json!({ "provider": providers })
}

/// The `options` block pointing one provider at the gateway.
fn gateway_options(base_url: &str, api_key: Option<&str>, auto_route: bool) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    options.insert(
        "baseURL".to_string(),
        serde_json::Value::String(format!("{base_url}/v1")),
    );
    if let Some(key) = api_key {
        options.insert(
            "apiKey".to_string(),
            serde_json::Value::String(key.to_string()),
        );
    }
    options.insert(
        "headers".to_string(),
        serde_json::json!({
            "x-gw-client": "opencode",
            "x-gw-auto-route": if auto_route { "1" } else { "0" },
        }),
    );
    serde_json::Value::Object(options)
}

/// Ask the running gateway for its model list and confirm every name we are
/// about to hand opencode is present.
///
/// Returns the names that are missing.
pub async fn verify_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    wanted: &[String],
) -> Result<Vec<String>> {
    let mut req = http
        .get(format!("{base_url}/v1/models"))
        .timeout(Duration::from_secs(5));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let body: serde_json::Value = req.send().await?.json().await?;
    let present: HashSet<&str> = body
        .get("data")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
        .collect();

    Ok(wanted
        .iter()
        .filter(|m| !present.contains(m.as_str()))
        .cloned()
        .collect())
}

/// Provider ids that stay a silent bypass no matter what `overrideProviders`
/// says, and why — see `crate::config::default_opencode_overrides` for the
/// full reasoning. Kept separate from that list because the warning here
/// needs to say something different for these: "adding it won't help",
/// rather than "it's missing, add it".
fn never_redirectable_reason(provider: &str) -> Option<&'static str> {
    match provider {
        "google" => Some(
            "@ai-sdk/google は /v1/models/{id}:generateContent という、\
             このゲートウェイのどのルートとも一致しないパスに投げるため",
        ),
        "github-copilot" => {
            Some("opencode 内製の SDK と認証プラグインを使っており、投げ先が固定されていないため")
        }
        "ollama" => Some("models.dev に ollama という provider id 自体が存在しないため"),
        _ => None,
    }
}

/// One warning line for a single `provider/model` pin, or `None` when that
/// provider is already redirected.
///
/// `location` identifies the file (and, when known, the agent within it) in
/// the message so a user can go fix it without grepping.
fn pin_warning(
    location: &str,
    agent: Option<&str>,
    provider: &str,
    override_providers: &[String],
) -> Option<String> {
    if override_providers.iter().any(|p| p == provider) {
        return None;
    }

    let who = match agent {
        Some(name) => format!("{location} のエージェント `{name}`"),
        None => location.to_string(),
    };

    Some(match never_redirectable_reason(provider) {
        Some(reason) => format!(
            "{who} がプロバイダ `{provider}` を pin していますが、gateway を経由せず直接 \
             {provider} に届きます。overrideProviders に `{provider}` を足しても動作しません\
             （{reason}）。gateway 経由にしたい場合は、ルート定義の model にこのプロバイダの \
             モデルを設定し、エージェント側は `gateway/…` を参照するように変更してください。"
        ),
        None => format!(
            "{who} がプロバイダ `{provider}` を pin していますが、launch.opencode.overrideProviders \
             に `{provider}` が含まれていないため gateway を経由せず直接 {provider} に届きます \
             （エラーは出ません）。overrideProviders に `{provider}` を追加してください。"
        ),
    })
}

/// Extract the `model:` value from an opencode agent file's YAML frontmatter.
///
/// Not a real YAML parser — this project has no YAML dependency, and the
/// only thing this needs to survive is a flat `key: value` line between two
/// `---` fences, which is all opencode agent frontmatter is documented to
/// be. A file that opens with anything other than `---` on line one, or has
/// no `model:` line before the closing fence, yields `None`.
fn frontmatter_model(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("model:") {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                return None;
            }
            return Some(value.to_string());
        }
    }
    None
}

/// `~/.config/opencode` — same base strategy `crate::paths` uses for this
/// gateway's own config dir, applied to opencode's app name instead.
fn opencode_global_dir() -> Option<PathBuf> {
    use etcetera::BaseStrategy;
    etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.config_dir().join("opencode"))
}

/// Every directory opencode scans for markdown agent definitions: project
/// (`.opencode/`, relative to the current directory — the same place
/// `launch opencode` itself runs from) and global (`~/.config/opencode/`),
/// each under both the current plural directory name and the singular one
/// opencode still supports for backwards compatibility.
fn agent_scan_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from(".opencode/agent"),
        PathBuf::from(".opencode/agents"),
    ];
    if let Some(global) = opencode_global_dir() {
        dirs.push(global.join("agent"));
        dirs.push(global.join("agents"));
    }
    dirs
}

/// Every `opencode.json` that can pin a model directly: project and global.
fn config_scan_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from("opencode.json")];
    if let Some(global) = opencode_global_dir() {
        files.push(global.join("opencode.json"));
    }
    files
}

fn scan_agent_dir(dir: &Path, override_providers: &[String], warnings: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(model) = frontmatter_model(&text) else {
            continue;
        };
        let Some((provider, _)) = model.split_once('/') else {
            continue;
        };
        let agent_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        if let Some(msg) = pin_warning(
            &path.display().to_string(),
            Some(agent_name),
            provider,
            override_providers,
        ) {
            warnings.push(msg);
        }
    }
}

fn scan_config_file(path: &Path, override_providers: &[String], warnings: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            warnings.push(format!(
                "{} がパースできません（不正なJSON）。pin の検出をスキップしました",
                path.display()
            ));
            return;
        }
    };

    if let Some(model) = parsed.get("model").and_then(|v| v.as_str()) {
        if let Some((provider, _)) = model.split_once('/') {
            if let Some(msg) = pin_warning(
                &path.display().to_string(),
                None,
                provider,
                override_providers,
            ) {
                warnings.push(msg);
            }
        }
    }

    if let Some(agents) = parsed.get("agent").and_then(|v| v.as_object()) {
        for (name, def) in agents {
            let Some(model) = def.get("model").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some((provider, _)) = model.split_once('/') else {
                continue;
            };
            if let Some(msg) = pin_warning(
                &path.display().to_string(),
                Some(name),
                provider,
                override_providers,
            ) {
                warnings.push(msg);
            }
        }
    }
}

/// Read-only scan for agent/config model pins that `overrideProviders`
/// cannot see through, i.e. that will silently bypass the gateway. Missing
/// files and directories are not errors — most setups have neither an agent
/// directory nor an `opencode.json`.
pub fn detect_pinned_bypasses(override_providers: &[String]) -> Vec<String> {
    detect_pinned_bypasses_in(&agent_scan_dirs(), &config_scan_files(), override_providers)
}

/// The actual scan, taking explicit paths so it can be exercised against a
/// temp directory in tests without touching the real project or
/// `~/.config/opencode`.
fn detect_pinned_bypasses_in(
    agent_dirs: &[PathBuf],
    config_files: &[PathBuf],
    override_providers: &[String],
) -> Vec<String> {
    let mut warnings = Vec::new();
    for dir in agent_dirs {
        scan_agent_dir(dir, override_providers, &mut warnings);
    }
    for file in config_files {
        scan_config_file(file, override_providers, &mut warnings);
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_field_is_present_only_when_configured() {
        let with_key = config_content("http://127.0.0.1:4000", Some("secret"), &[], &[], true);
        assert_eq!(
            with_key["provider"]["gateway"]["options"]["apiKey"],
            serde_json::json!("secret")
        );

        let without_key = config_content("http://127.0.0.1:4000", None, &[], &[], true);
        assert!(without_key["provider"]["gateway"]["options"]
            .get("apiKey")
            .is_none());
    }

    /// The bypass-closing behaviour: built-in providers named in
    /// `overrideProviders` get their `baseURL` pointed at the gateway, but keep
    /// their `npm` untouched (config merge preserves the native SDK, and with
    /// it the wire protocol the gateway expects on that endpoint).
    #[test]
    fn override_providers_are_redirected_without_replacing_their_sdk() {
        let overrides = vec!["openai".to_string(), "anthropic".to_string()];
        let content = config_content("http://127.0.0.1:4000", Some("k"), &[], &overrides, true);

        for id in ["openai", "anthropic"] {
            assert_eq!(
                content["provider"][id]["options"]["baseURL"], "http://127.0.0.1:4000/v1",
                "{id} must point at the gateway"
            );
            assert_eq!(content["provider"][id]["options"]["apiKey"], "k");
            assert!(
                content["provider"][id].get("npm").is_none(),
                "{id} must keep its native npm package"
            );
        }
        // The gateway provider itself is unaffected.
        assert_eq!(
            content["provider"]["gateway"]["npm"],
            "@ai-sdk/openai-compatible"
        );
    }

    #[test]
    fn config_content_shape_matches_expected_fields() {
        let content = config_content(
            "http://127.0.0.1:4000",
            None,
            &["route-a".to_string(), "route-b".to_string()],
            &[],
            true,
        );

        assert_eq!(
            content["provider"]["gateway"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            content["provider"]["gateway"]["options"]["baseURL"],
            "http://127.0.0.1:4000/v1"
        );
        assert_eq!(
            content["provider"]["gateway"]["options"]["headers"]["x-gw-client"],
            "opencode"
        );
        assert_eq!(
            content["provider"]["gateway"]["options"]["headers"]["x-gw-auto-route"],
            "1"
        );
        assert_eq!(
            content["provider"]["gateway"]["models"]["route-a"],
            serde_json::json!({})
        );
        assert_eq!(
            content["provider"]["gateway"]["models"]["route-b"],
            serde_json::json!({})
        );
    }

    #[test]
    fn auto_route_false_is_reflected_in_the_header() {
        let content = config_content("http://127.0.0.1:4000", None, &[], &[], false);
        assert_eq!(
            content["provider"]["gateway"]["options"]["headers"]["x-gw-auto-route"],
            "0"
        );
    }

    #[test]
    fn build_produces_expected_args() {
        let config = Config::default();

        let invocation = build(
            &config,
            "route-a",
            &["route-a".to_string()],
            false,
            true,
            &["--foo".to_string()],
        )
        .unwrap();

        assert_eq!(invocation.program, "opencode");
        assert_eq!(
            invocation.args,
            vec![
                "-m".to_string(),
                "gateway/route-a".to_string(),
                "--foo".to_string()
            ]
        );
    }

    #[test]
    fn isolate_adds_pure_flag() {
        let config = Config::default();

        let invocation = build(
            &config,
            "route-a",
            &["route-a".to_string()],
            true,
            true,
            &[],
        )
        .unwrap();

        assert!(invocation.args.contains(&"--pure".to_string()));
    }

    #[test]
    fn empty_models_falls_back_to_listable_routes() {
        let mut config = Config::default();
        config.routes.insert(
            "route-a".to_string(),
            crate::config::RouteConfig {
                model: crate::config::ModelConfig {
                    default: "p/m".to_string(),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );

        let resolved = resolved_models(&config, &[]);
        assert_eq!(resolved, vec!["route-a".to_string()]);
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn missing_agent_dir_and_config_file_report_no_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().join("no-such-agent-dir")],
            &[dir.path().join("no-such-config.json")],
            &default_opencode_overrides(),
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn agent_frontmatter_without_model_is_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "researcher.md",
            "---\ndescription: does research\n---\nbody",
        );
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().to_path_buf()],
            &[],
            &default_opencode_overrides(),
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn model_without_provider_prefix_is_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "researcher.md", "---\nmodel: gpt-4\n---\nbody");
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().to_path_buf()],
            &[],
            &default_opencode_overrides(),
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn redirected_provider_pin_is_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "researcher.md",
            "---\nmodel: openai/gpt-5\n---\nbody",
        );
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().to_path_buf()],
            &[],
            &default_opencode_overrides(),
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn non_redirected_provider_pin_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "researcher.md",
            "---\nmodel: fireworks/some-model\n---\nbody",
        );
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().to_path_buf()],
            &[],
            &default_opencode_overrides(),
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("researcher"));
        assert!(warnings[0].contains("fireworks"));
        assert!(warnings[0].contains("overrideProviders"));
    }

    #[test]
    fn never_redirectable_provider_gets_a_dedicated_message() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "researcher.md",
            "---\nmodel: google/gemini-3-pro\n---\nbody",
        );
        let warnings = detect_pinned_bypasses_in(
            &[dir.path().to_path_buf()],
            &[],
            &default_opencode_overrides(),
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("足しても動作しません"));
        assert!(warnings[0].contains("google"));
    }

    #[test]
    fn broken_opencode_json_reports_a_single_warning() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "opencode.json", "{ not json");
        let warnings = detect_pinned_bypasses_in(&[], &[file], &default_opencode_overrides());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("パースできません"));
    }

    #[test]
    fn opencode_json_top_level_and_agent_model_pins_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(
            dir.path(),
            "opencode.json",
            r#"{
                "model": "google/gemini-3-pro",
                "agent": {
                    "reviewer": { "model": "fireworks/some-model" },
                    "writer": { "model": "openai/gpt-5" }
                }
            }"#,
        );
        let warnings = detect_pinned_bypasses_in(&[], &[file], &default_opencode_overrides());
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("google")));
        assert!(warnings
            .iter()
            .any(|w| w.contains("reviewer") && w.contains("fireworks")));
    }

    #[test]
    fn frontmatter_model_requires_leading_fence() {
        assert_eq!(frontmatter_model("model: openai/gpt-5\nbody"), None);
    }
}
