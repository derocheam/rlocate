//! Core library for `rlocate`, a cross-platform Rust `plocate`.
//!
//! This crate contains everything that is shared between the indexer
//! (`rlocate-updatedb`) and the searcher (`rlocate`):
//!
//! * [`trigram`] — trigram extraction and query decomposition
//! * [`postings`] — delta + varint posting-list codec and intersection
//! * [`names`] — the zstd-compressed filename block store
//! * [`matcher`] — substring / glob / regex / basename matching
//! * [`index`] — the on-disk index format (reader + writer)
//! * [`config`] — `updatedb.conf`-style prune configuration
//! * [`fs`] — cross-platform directory walking and mount pruning

pub mod config;
pub mod fs;
pub mod index;
pub mod matcher;
pub mod names;
pub mod net;
pub mod postings;
pub mod remotes;
pub mod search;
pub mod trigram;

/// Default number of filenames stored per compressed block.
pub const BLOCK_SIZE: usize = 32;

/// Magic bytes at the start of every `rlocate.db`.
pub const MAGIC: [u8; 4] = *b"RLOC";

/// On-disk format version. Bump on any incompatible layout change.
pub const FORMAT_VERSION: u32 = 1;

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not an rlocate index (bad magic)")]
    BadMagic,
    #[error("unsupported index format version {found}, this build expects {expected}")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("index file is truncated or corrupt: {0}")]
    Corrupt(&'static str),
    #[error("invalid pattern: {0}")]
    Pattern(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The default index location for this platform.
///
/// System-wide path when writable (root), else a per-user cache path so an
/// unprivileged build still works.
pub fn default_db_path() -> std::path::PathBuf {
    use std::path::PathBuf;

    #[cfg(target_os = "macos")]
    let system = PathBuf::from("/var/db/rlocate/rlocate.db");
    #[cfg(not(target_os = "macos"))]
    let system = PathBuf::from("/var/lib/rlocate/rlocate.db");

    // Prefer the system path for root; otherwise fall back to a user cache.
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe.
        let is_root = unsafe { libc_getuid() } == 0;
        if is_root {
            return system;
        }
    }

    if let Some(dir) = user_cache_dir() {
        return dir.join("rlocate").join("rlocate.db");
    }
    system
}

#[cfg(unix)]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn user_cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    let home = std::env::var_os("HOME")?;
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(home).join("Library").join("Caches"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(PathBuf::from(home).join(".cache"))
    }
}
