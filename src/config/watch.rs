//! Hot reload.
//!
//! The gateway is expected to be up when a 09:01 cron job fires on another
//! machine, so a bad edit must never take it down: if a reload fails to parse or
//! validate, the previous config keeps serving and the error goes to stderr.
//!
//! Readers hold an `Arc<Config>` for the life of a request, so a swap mid-flight
//! is harmless — in-flight requests finish against the config they started with.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::error::Result;

/// A config that can be replaced while the server is running.
pub struct SharedConfig {
    pub(crate) current: ArcSwap<Config>,
    pub(crate) path: PathBuf,
}

impl SharedConfig {
    /// Load once and prepare for later reloads.
    pub fn load(path: PathBuf) -> Result<Arc<Self>> {
        let _ = path;
        todo!("src/config/watch.rs")
    }

    /// Build from an already-parsed config. Used by tests.
    pub fn from_config(config: Config, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            current: ArcSwap::from_pointee(config),
            path,
        })
    }

    /// The config as of right now. Cheap and lock-free.
    pub fn get(&self) -> Arc<Config> {
        self.current.load_full()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read and re-validate, replacing the current config only on success.
    ///
    /// Returns the human-readable summary of what changed, or `None` when the
    /// reload was rejected (in which case the error has already been logged and
    /// the old config is still live).
    pub fn reload(&self) -> Option<String> {
        todo!("src/config/watch.rs")
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
    let _ = shared;
    todo!("src/config/watch.rs")
}

/// Keeps the filesystem watcher alive.
pub struct WatchGuard {
    #[allow(dead_code)]
    pub(crate) inner: Box<dyn std::any::Any + Send + Sync>,
}
