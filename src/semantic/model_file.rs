//! Fetching and verifying the static embedding model used for semantic
//! routing.
//!
//! `model2vec-rs`'s own `hf-hub` feature could do this, but pulling it in
//! drags back the `dirs` crate (deliberately excluded, see
//! `docs/decisions.md`), a second HTTP stack (`ureq`) and an `indicatif`
//! progress bar — and `hf-hub` 0.4.3 ignores `HF_HOME`, hardcoding
//! `~/.cache/huggingface/hub` regardless of where this gateway keeps its
//! state. Fetching the three files ourselves with the existing `reqwest`
//! client avoids all of that.
//!
//! This module's job stops at "the files exist on disk and match the
//! checksums pinned below" — loading them into `model2vec-rs` is separate,
//! later work.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::paths::semantic_model_dir;

const BASE_URL: &str = "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/main";

/// How long to wait for HuggingFace to respond with headers before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a download may go without receiving any bytes before it is
/// considered stalled. `model.safetensors` is ~500MB, so this is generous —
/// it exists to catch a hung connection, not to bound total transfer time.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Paths to the three files the model needs, once [`ensure`] has returned.
pub struct ModelFiles {
    pub config: PathBuf,
    pub tokenizer: PathBuf,
    pub model: PathBuf,
}

/// One file to fetch, with size and (where available) sha256 pinned from the
/// HuggingFace API.
struct FileSpec {
    name: &'static str,
    size: u64,
    /// `None` for files HuggingFace does not track via Git LFS — for those,
    /// the API only exposes a git blob sha1 (`oid`), not a sha256, so size is
    /// the only check available.
    sha256: Option<&'static str>,
}

/// Pinned from `GET https://huggingface.co/api/models/minishlab/potion-multilingual-128M?blobs=true`
/// (checked 2026-07-30). `tokenizer.json` and `model.safetensors` are stored
/// via Git LFS, so the API reports `lfs.sha256` for them; `config.json` is a
/// plain (non-LFS) blob, so only its size is pinned.
const FILES: [FileSpec; 3] = [
    FileSpec {
        name: "config.json",
        size: 271,
        sha256: None,
    },
    FileSpec {
        name: "tokenizer.json",
        size: 18_616_131,
        sha256: Some("19f1909063da3cfe3bd83a782381f040dccea475f4816de11116444a73e1b6a1"),
    },
    FileSpec {
        name: "model.safetensors",
        size: 512_361_560,
        sha256: Some("14b5eb39cb4ce5666da8ad1f3dc6be4346e9b2d601c073302fa0a31bf7943397"),
    },
];

/// Make sure the embedding model is present and intact under
/// [`semantic_model_dir`], downloading whatever is missing or corrupt.
///
/// If all three files already exist with the right size and checksum, this
/// touches the network not at all.
pub async fn ensure(http: &reqwest::Client) -> Result<ModelFiles> {
    let dir = semantic_model_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(|source| {
        Error::Other(format!(
            "failed to create model directory {}: {source}\ncheck that the parent directory is writable",
            dir.display()
        ))
    })?;

    for spec in &FILES {
        let path = dir.join(spec.name);
        if verify_file(&path, spec).await {
            continue;
        }
        download(http, &dir, spec).await?;
    }

    Ok(ModelFiles {
        config: dir.join("config.json"),
        tokenizer: dir.join("tokenizer.json"),
        model: dir.join("model.safetensors"),
    })
}

/// `true` if `path` exists, has the expected size, and (when pinned) the
/// expected sha256.
async fn verify_file(path: &Path, spec: &FileSpec) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if metadata.len() != spec.size {
        return false;
    }
    match spec.sha256 {
        Some(expected) => matches!(sha256_file(path).await, Ok(actual) if actual == expected),
        None => true,
    }
}

async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Infallible: writing to a `String` never fails.
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Download `spec` into `dir`, writing through a `.partial` sibling and
/// renaming into place only once the size and checksum both check out.
///
/// The rename is what makes this atomic: a process killed mid-download
/// leaves behind only the `.partial` file, never a truncated file at the
/// final name, and the next [`ensure`] call ignores `.partial` files and
/// retries from scratch.
async fn download(http: &reqwest::Client, dir: &Path, spec: &FileSpec) -> Result<()> {
    let url = format!("{BASE_URL}/{}", spec.name);
    let final_path = dir.join(spec.name);
    let partial_path = dir.join(format!("{}.partial", spec.name));

    eprintln!(
        "llm-gateway: downloading {} ({:.1} MB) for semantic routing...",
        spec.name,
        spec.size as f64 / 1_000_000.0
    );

    let response = tokio::time::timeout(CONNECT_TIMEOUT, http.get(&url).send())
        .await
        .map_err(|_| {
            Error::Other(format!(
                "timed out connecting to HuggingFace to download {url}\ncheck network connectivity and retry; \
                 the download resumes from scratch, no partial data is kept"
            ))
        })?
        .map_err(|source| {
            Error::Other(format!(
                "failed to start downloading {url}: {source}\ncheck network connectivity and retry"
            ))
        })?;

    if !response.status().is_success() {
        return Err(Error::Other(format!(
            "failed to download {url}: HTTP {}\nthe file may have moved on HuggingFace; \
             check https://huggingface.co/minishlab/potion-multilingual-128M",
            response.status()
        )));
    }

    let mut file = tokio::fs::File::create(&partial_path)
        .await
        .map_err(|source| {
            Error::Other(format!(
                "failed to create {}: {source}\ncheck that {} is writable",
                partial_path.display(),
                dir.display()
            ))
        })?;

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut hasher = Sha256::new();
    let mut last_report = Instant::now();

    loop {
        let next = tokio::time::timeout(STALL_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                Error::Other(format!(
                    "download of {} stalled (no data for {}s)\nretry; \
                     the partial download is not kept, so the next attempt starts from scratch",
                    spec.name,
                    STALL_TIMEOUT.as_secs()
                ))
            })?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|source| {
            Error::Other(format!(
                "connection dropped while downloading {}: {source}\nretry",
                spec.name
            ))
        })?;

        file.write_all(chunk.as_ref()).await.map_err(|source| {
            Error::Other(format!(
                "failed to write {}: {source}",
                partial_path.display()
            ))
        })?;
        hasher.update(chunk.as_ref());
        downloaded += chunk.len() as u64;

        if last_report.elapsed() >= Duration::from_millis(200) {
            report_progress(spec.name, downloaded, spec.size);
            last_report = Instant::now();
        }
    }
    report_progress(spec.name, downloaded, spec.size);
    eprintln!();

    file.flush().await.map_err(|source| {
        Error::Other(format!(
            "failed to flush {}: {source}",
            partial_path.display()
        ))
    })?;
    drop(file);

    if downloaded != spec.size {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err(Error::Other(format!(
            "downloaded {downloaded} bytes for {}, expected {}\nthe download was cut short; retry",
            spec.name, spec.size
        )));
    }

    if let Some(expected) = spec.sha256 {
        let actual = to_hex(&hasher.finalize());
        if actual != expected {
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err(Error::Other(format!(
                "checksum mismatch for {}: expected sha256 {expected}, got {actual}\n\
                 the downloaded file was discarded; retry, or the pinned checksum in \
                 src/semantic/model_file.rs may be stale",
                spec.name
            )));
        }
    }

    tokio::fs::rename(&partial_path, &final_path)
        .await
        .map_err(|source| {
            Error::Other(format!(
                "downloaded {} but failed to move it into place at {}: {source}",
                spec.name,
                final_path.display()
            ))
        })?;

    Ok(())
}

fn report_progress(name: &str, downloaded: u64, total: u64) {
    let pct = (downloaded as f64 / total as f64) * 100.0;
    eprint!("\r  {name}: {pct:5.1}%");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hex_matches_known_vector() {
        // sha256("") — a fixed, well-known vector to catch a broken encoder,
        // independent of any network access.
        let empty = Sha256::digest(b"");
        assert_eq!(
            to_hex(&empty),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn file_specs_match_the_pinned_sizes_from_the_hf_api() {
        // Regression guard: these numbers came from the HF blobs API and the
        // issue's own research, not from this code — a typo here should fail
        // loudly rather than silently accepting a corrupt/truncated file.
        let by_name = |n: &str| FILES.iter().find(|f| f.name == n).unwrap();
        assert_eq!(by_name("config.json").size, 271);
        assert_eq!(by_name("tokenizer.json").size, 18_616_131);
        assert_eq!(by_name("model.safetensors").size, 512_361_560);
        assert!(by_name("config.json").sha256.is_none());
        assert_eq!(
            by_name("tokenizer.json").sha256.unwrap().len(),
            64,
            "sha256 hex digests are 64 chars"
        );
        assert_eq!(by_name("model.safetensors").sha256.unwrap().len(), 64);
    }

    #[tokio::test]
    async fn verify_file_rejects_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &FILES[0];
        assert!(!verify_file(&dir.path().join(spec.name), spec).await);
    }

    #[tokio::test]
    async fn verify_file_rejects_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &FILES[0]; // config.json, size 271
        let path = dir.path().join(spec.name);
        tokio::fs::write(&path, vec![0u8; 10]).await.unwrap();
        assert!(!verify_file(&path, spec).await);
    }

    #[tokio::test]
    async fn verify_file_rejects_right_size_wrong_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &FILES[1]; // tokenizer.json, has a pinned sha256
        let path = dir.path().join(spec.name);
        tokio::fs::write(&path, vec![0u8; spec.size as usize])
            .await
            .unwrap();
        assert!(!verify_file(&path, spec).await);
    }

    #[tokio::test]
    async fn verify_file_accepts_matching_size_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &FILES[0]; // config.json — size-only check
        let path = dir.path().join(spec.name);
        tokio::fs::write(&path, vec![0u8; spec.size as usize])
            .await
            .unwrap();
        assert!(verify_file(&path, spec).await);
    }

    #[tokio::test]
    async fn a_partial_file_left_from_an_earlier_download_is_never_mistaken_for_the_real_thing() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &FILES[0];
        // Simulate a previous run that got interrupted mid-download: only
        // the `.partial` sibling exists, not the final name.
        tokio::fs::write(
            dir.path().join(format!("{}.partial", spec.name)),
            vec![0u8; spec.size as usize],
        )
        .await
        .unwrap();
        assert!(!verify_file(&dir.path().join(spec.name), spec).await);
    }

    #[test]
    fn semantic_model_dir_is_under_the_config_dir() {
        let dir = semantic_model_dir();
        assert!(dir.ends_with("models/potion-multilingual-128M"));
    }
}
