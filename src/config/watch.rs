//! Hot reload.
//!
//! The gateway is expected to be up when a 09:01 cron job fires on another
//! machine, so a bad edit must never take it down: if a reload fails to parse or
//! validate, the previous config keeps serving and the error goes to stderr.
//!
//! Readers hold an `Arc<Config>` for the life of a request, so a swap mid-flight
//! is harmless — in-flight requests finish against the config they started with.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;

/// A config that can be replaced while the server is running.
pub struct SharedConfig {
    pub(crate) current: ArcSwap<Config>,
    pub(crate) path: PathBuf,
    /// Bumped each time `reload` swaps in a new config. Lets a consumer that
    /// caches something derived from the config (the semantic routing index,
    /// see `crate::semantic::index`) tell whether its cache is still current
    /// without re-deriving it on every use — it compares this against the
    /// value it last built from and only rebuilds when the two disagree.
    generation: AtomicU64,
}

impl SharedConfig {
    /// Load once and prepare for later reloads.
    pub fn load(path: PathBuf) -> Result<Arc<Self>> {
        let config = Config::load_from(&path)?;
        Ok(Arc::new(Self {
            current: ArcSwap::from_pointee(config),
            path,
            generation: AtomicU64::new(0),
        }))
    }

    /// Build from an already-parsed config. Used by tests.
    #[allow(dead_code)]
    pub fn from_config(config: Config, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            current: ArcSwap::from_pointee(config),
            path,
            generation: AtomicU64::new(0),
        })
    }

    /// The config as of right now. Cheap and lock-free.
    pub fn get(&self) -> Arc<Config> {
        self.current.load_full()
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current generation counter. Starts at 0 and increases by 1 on every
    /// config swap made by `reload` — never on the initial load, so a
    /// consumer that builds its cache from the config passed to it at
    /// construction time and records generation 0 stays in step without a
    /// spurious rebuild on its first use.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Re-read and re-validate, replacing the current config only on success.
    ///
    /// Returns the human-readable summary of what changed, or `None` when the
    /// reload was rejected (in which case the error has already been logged and
    /// the old config is still live).
    pub fn reload(&self) -> Option<String> {
        let new_config = match Config::load_from(&self.path) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(
                    "config reload from {} failed, keeping previous config: {err}",
                    self.path.display()
                );
                return None;
            }
        };

        let old_config = self.current.load_full();
        let summary = summarize_change(&old_config, &new_config);
        self.current.store(Arc::new(new_config));
        // Ordered after the config store above (both are SeqCst, and this
        // is the only writer): a reader that observes the new generation
        // via `generation()` is guaranteed to observe the new config too if
        // it then calls `get()`, so nothing can be caught mid-swap holding a
        // generation number that outruns the config it describes.
        self.generation.fetch_add(1, Ordering::SeqCst);
        tracing::info!("{summary}");
        Some(summary)
    }
}

/// Describe what changed between two configs in one line, for the reload log.
///
/// Extractive rather than exhaustive: it names routes and providers that were
/// added, removed or changed model, and falls back to a generic message when
/// nothing it looks for moved (e.g. only `server` or `logging` changed).
fn summarize_change(old: &Config, new: &Config) -> String {
    let mut parts = Vec::new();

    let added_routes: Vec<&str> = new
        .routes
        .keys()
        .filter(|name| !old.routes.contains_key(*name))
        .map(String::as_str)
        .collect();
    let removed_routes: Vec<&str> = old
        .routes
        .keys()
        .filter(|name| !new.routes.contains_key(*name))
        .map(String::as_str)
        .collect();
    let changed_routes: Vec<&str> = new
        .routes
        .iter()
        .filter_map(|(name, route)| {
            let old_route = old.routes.get(name)?;
            let changed = old_route.model.default != route.model.default
                || old_route.model.fallbacks != route.model.fallbacks;
            changed.then_some(name.as_str())
        })
        .collect();

    if !added_routes.is_empty() {
        parts.push(format!("added route(s) {}", added_routes.join(", ")));
    }
    if !removed_routes.is_empty() {
        parts.push(format!("removed route(s) {}", removed_routes.join(", ")));
    }
    if !changed_routes.is_empty() {
        parts.push(format!(
            "changed model for route(s) {}",
            changed_routes.join(", ")
        ));
    }

    let added_providers: Vec<&str> = new
        .providers
        .keys()
        .filter(|id| !old.providers.contains_key(*id))
        .map(String::as_str)
        .collect();
    let removed_providers: Vec<&str> = old
        .providers
        .keys()
        .filter(|id| !new.providers.contains_key(*id))
        .map(String::as_str)
        .collect();

    if !added_providers.is_empty() {
        parts.push(format!("added provider(s) {}", added_providers.join(", ")));
    }
    if !removed_providers.is_empty() {
        parts.push(format!(
            "removed provider(s) {}",
            removed_providers.join(", ")
        ));
    }

    if parts.is_empty() {
        "config reloaded".to_string()
    } else {
        parts.join("; ")
    }
}

/// Watch `config.json` and every file referenced by a `description`, replacing
/// the shared config when they change.
///
/// Uses a debouncer because an editor's atomic save (write temp + rename) emits
/// a sequence of events that differs between editors and between platforms;
/// reacting to each one would reload several times per save.
///
/// Returns a guard — dropping it stops watching.
pub fn spawn(shared: Arc<SharedConfig>) -> Result<WatchGuard> {
    let config_path = shared.path.clone();
    let config_file_name = config_path.file_name().map(|name| name.to_owned());
    let watch_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let llm_dir = paths::llm_dir();

    let handler_shared = Arc::clone(&shared);
    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        None,
        move |result: DebounceEventResult| {
            let events = match result {
                Ok(events) => events,
                Err(errors) => {
                    for error in errors {
                        tracing::error!("config watcher error: {error}");
                    }
                    return;
                }
            };

            let relevant = events.iter().any(|event| {
                event
                    .paths
                    .iter()
                    .any(|path| is_relevant(path, config_file_name.as_deref(), &llm_dir))
            });

            if relevant {
                handler_shared.reload();
            }
        },
    )
    .map_err(|err| Error::Other(format!("failed to start config watcher: {err}")))?;

    debouncer
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|err| Error::Other(format!("failed to watch {}: {err}", watch_dir.display())))?;

    if paths::llm_dir().exists() {
        debouncer
            .watch(paths::llm_dir(), RecursiveMode::Recursive)
            .map_err(|err| {
                Error::Other(format!(
                    "failed to watch {}: {err}",
                    paths::llm_dir().display()
                ))
            })?;
    }

    Ok(WatchGuard {
        inner: Box::new(debouncer),
    })
}

/// Whether a changed path should trigger a reload: either it is the config
/// file itself (matched by name, since the watched root is its parent
/// directory so atomic-save renames are still caught), or it falls under
/// `llm/`, where `description` file bodies live.
fn is_relevant(path: &Path, config_file_name: Option<&std::ffi::OsStr>, llm_dir: &Path) -> bool {
    if let Some(name) = config_file_name {
        if path.file_name() == Some(name) {
            return true;
        }
    }
    path.starts_with(llm_dir)
}

/// Keeps the filesystem watcher alive.
pub struct WatchGuard {
    #[allow(dead_code)]
    pub(crate) inner: Box<dyn std::any::Any + Send + Sync>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_CONFIG: &str = r#"{
        providers: {
            anthropic: {
                baseUrl: "https://api.anthropic.test",
                api: "anthropic-messages",
            },
        },
        routes: {
            "role-writer": {
                description: "a test route",
                model: { default: "anthropic/opus-pinned" },
            },
            "default": {
                description: "catch-all",
                model: { default: "anthropic/opus-pinned" },
            },
        },
    }"#;

    const VALID_CONFIG_CHANGED: &str = r#"{
        providers: {
            anthropic: {
                baseUrl: "https://api.anthropic.test",
                api: "anthropic-messages",
            },
        },
        routes: {
            "role-writer": {
                description: "a test route",
                model: { default: "anthropic/opus-updated" },
            },
            "default": {
                description: "catch-all",
                model: { default: "anthropic/opus-pinned" },
            },
        },
    }"#;

    fn write_config(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join("config.json");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn from_config_exposes_the_given_config_and_path() {
        let path = PathBuf::from("/nonexistent/llm-gateway-config.json");
        let shared = SharedConfig::from_config(Config::default(), path.clone());

        assert_eq!(shared.path(), path.as_path());
        assert!(shared.get().routes.is_empty());
    }

    #[test]
    fn load_succeeds_on_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, VALID_CONFIG);

        let shared = SharedConfig::load(path).unwrap();
        assert_eq!(
            shared.get().routes["role-writer"].model.default,
            "anthropic/opus-pinned"
        );
    }

    #[test]
    fn reload_keeps_old_config_when_the_new_one_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, VALID_CONFIG);
        let shared = SharedConfig::load(path.clone()).unwrap();

        std::fs::write(&path, b"{ this is not valid json5 ").unwrap();

        assert!(shared.reload().is_none());
        assert_eq!(
            shared.get().routes["role-writer"].model.default,
            "anthropic/opus-pinned"
        );
    }

    #[test]
    fn reload_swaps_in_a_valid_change_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, VALID_CONFIG);
        let shared = SharedConfig::load(path.clone()).unwrap();

        std::fs::write(&path, VALID_CONFIG_CHANGED).unwrap();

        let summary = shared.reload();
        assert!(summary.is_some(), "{summary:?}");
        assert_eq!(
            shared.get().routes["role-writer"].model.default,
            "anthropic/opus-updated"
        );
    }

    #[test]
    fn generation_starts_at_zero_and_does_not_move_on_initial_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, VALID_CONFIG);
        let shared = SharedConfig::load(path).unwrap();

        assert_eq!(shared.generation(), 0);
    }

    #[test]
    fn generation_advances_only_on_a_successful_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, VALID_CONFIG);
        let shared = SharedConfig::load(path.clone()).unwrap();
        assert_eq!(shared.generation(), 0);

        // A reload that fails to parse must not move the counter — a
        // consumer must not think there is something new to rebuild from.
        std::fs::write(&path, b"{ this is not valid json5 ").unwrap();
        assert!(shared.reload().is_none());
        assert_eq!(shared.generation(), 0);

        // A reload that succeeds must move it, exactly once.
        std::fs::write(&path, VALID_CONFIG_CHANGED).unwrap();
        assert!(shared.reload().is_some());
        assert_eq!(shared.generation(), 1);

        std::fs::write(&path, VALID_CONFIG).unwrap();
        assert!(shared.reload().is_some());
        assert_eq!(shared.generation(), 2);
    }
}
