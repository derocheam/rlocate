//! Cross-platform filesystem walking.
//!
//! Produces the full-path byte strings that become index documents. The walk:
//!
//! * does not follow symlinks (directories reached via a symlink are recorded
//!   but not descended), matching `plocate`/`mlocate`;
//! * honours [`PruneConfig`] path and name rules;
//! * detects mount boundaries by device-id change and, at each boundary, prunes
//!   filesystems whose type is in [`PruneConfig::fs_types`] (type names come
//!   from the platform [`Mounts`] table); with
//!   [`PruneConfig::cross_mounts`]` == false` it stays on one filesystem.

use crate::config::PruneConfig;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// A snapshot of mounted filesystems used to look up a path's filesystem type.
pub struct Mounts {
    /// `(mountpoint, fs_type)` sorted by descending mountpoint depth so the
    /// first prefix match is the most specific (deepest) mount.
    entries: Vec<(PathBuf, String)>,
}

impl Mounts {
    /// Load the current mount table for this platform.
    pub fn load() -> Self {
        #[cfg(target_os = "linux")]
        let mut entries = linux::read_mounts();
        #[cfg(target_os = "macos")]
        let mut entries = macos::read_mounts();
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let mut entries: Vec<(PathBuf, String)> = Vec::new();

        entries.sort_by_key(|(mp, _)| std::cmp::Reverse(mp.components().count()));
        Self { entries }
    }

    /// The filesystem type of the mount containing `path`, if known.
    pub fn fs_type(&self, path: &Path) -> Option<&str> {
        self.entries
            .iter()
            .find(|(mp, _)| path.starts_with(mp))
            .map(|(_, ty)| ty.as_str())
    }
}

/// Walk `roots`, returning every discovered path as raw bytes (unsorted).
pub fn walk(roots: &[PathBuf], cfg: &PruneConfig) -> Vec<Vec<u8>> {
    let mounts = Mounts::load();
    let mut out = Vec::new();
    for root in roots {
        let root_dev = fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);
        walk_one(root, root_dev, cfg, &mounts, &mut out);
    }
    out
}

fn walk_one(
    root: &Path,
    root_dev: u64,
    cfg: &PruneConfig,
    mounts: &Mounts,
    out: &mut Vec<Vec<u8>>,
) {
    // Record the root itself.
    out.push(root.as_os_str().as_bytes().to_vec());

    // Stack holds (directory, that directory's device id) so a child on a
    // different device is recognized as a mount boundary.
    let mut stack: Vec<(PathBuf, u64)> = vec![(root.to_path_buf(), root_dev)];
    while let Some((dir, dir_dev)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir: skip quietly (permissions, races)
        };
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.as_os_str().as_bytes().to_vec());

            // Only descend into real directories (not symlinks).
            let meta = match entry.file_type() {
                Ok(ft) if ft.is_dir() => match fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                },
                _ => continue,
            };

            // Prune by basename.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if cfg.should_prune_name(name) {
                    continue;
                }
            }
            // Prune by absolute path.
            if let Some(s) = path.to_str() {
                if cfg.should_prune_path(s) {
                    continue;
                }
            }

            // Mount-boundary handling.
            let child_dev = meta.dev();
            if child_dev != dir_dev {
                if !cfg.cross_mounts {
                    continue; // one-file-system mode
                }
                if let Some(fs_type) = mounts.fs_type(&path) {
                    if cfg.fs_types.contains(fs_type) {
                        continue; // e.g. network / pseudo filesystem
                    }
                }
            }
            stack.push((path, child_dev));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn walks_and_prunes_names() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::create_dir_all(base.join(".git/objects")).unwrap();
        fs::write(base.join("a/file.txt"), b"x").unwrap();
        fs::write(base.join(".git/config"), b"x").unwrap();

        let cfg = PruneConfig {
            names: [".git".to_string()].into_iter().collect(),
            paths: Default::default(),
            fs_types: Default::default(),
            cross_mounts: true,
        };
        let found: HashSet<String> = walk(&[base.to_path_buf()], &cfg)
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();

        let file = base.join("a/file.txt").to_string_lossy().into_owned();
        let git_cfg = base.join(".git/config").to_string_lossy().into_owned();
        assert!(found.contains(&file));
        // .git itself is recorded but not descended, so its contents are absent.
        assert!(!found.contains(&git_cfg));
    }
}
