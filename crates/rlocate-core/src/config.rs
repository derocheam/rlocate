//! `updatedb.conf`-style prune configuration.
//!
//! Mirrors the GNU `updatedb` knobs: `PRUNEPATHS` (absolute directories to
//! skip), `PRUNENAMES` (directory basenames to skip anywhere), and `PRUNEFS`
//! (filesystem types to skip — honoured by the platform mount logic in
//! milestone M4). Defaults differ per OS.

use std::collections::HashSet;

/// Prune rules applied during a filesystem walk.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Absolute paths whose subtrees are skipped entirely.
    pub paths: HashSet<String>,
    /// Directory basenames skipped wherever they occur.
    pub names: HashSet<String>,
    /// Lowercase filesystem type names to skip (used in M4 mount handling).
    pub fs_types: HashSet<String>,
    /// Whether the walk may cross filesystem (mount) boundaries.
    pub cross_mounts: bool,
}

impl PruneConfig {
    /// Platform defaults.
    pub fn platform_default() -> Self {
        let names: HashSet<String> = [".git", ".hg", ".svn", ".bzr", "CVS"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        #[cfg(target_os = "macos")]
        let (paths, fs_types) = (
            vec![
                "/private/var/vm",
                // On APFS, /Users, /Applications, etc. are firmlinks into the
                // Data volume, so the whole Data volume is already reachable
                // from "/". Pruning its direct mountpoint avoids walking it twice.
                "/System/Volumes/Data",
                "/System/Volumes/VM",
                "/System/Volumes/Preboot",
                "/System/Volumes/Recovery",
                "/System/Volumes/Update",
                "/System/Volumes/Hardware",
                "/System/Volumes/iSCPreboot",
                "/System/Volumes/xarts",
                "/Volumes",
            ],
            vec![
                "devfs", "autofs", "nfs", "smbfs", "afpfs", "msdos", "webdav", "fuse",
            ],
        );

        #[cfg(not(target_os = "macos"))]
        let (paths, fs_types) = (
            vec![
                "/proc",
                "/sys",
                "/dev",
                "/run",
                "/tmp",
                "/var/tmp",
                "/var/run",
                "/var/lock",
                "/var/cache",
            ],
            vec![
                "proc",
                "sysfs",
                "devtmpfs",
                "tmpfs",
                "nfs",
                "nfs4",
                "cifs",
                "smbfs",
                "autofs",
                "devpts",
                "afs",
                "fuse.sshfs",
            ],
        );

        Self {
            paths: paths.into_iter().map(|s| s.to_string()).collect(),
            names,
            fs_types: fs_types.into_iter().map(|s| s.to_string()).collect(),
            // Index all local filesystems by default (like mlocate/plocate),
            // relying on `fs_types` to skip network/pseudo filesystems.
            cross_mounts: true,
        }
    }

    /// True if an absolute directory `path` should be pruned.
    pub fn should_prune_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    /// True if a directory basename `name` should be pruned.
    pub fn should_prune_name(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Parse an `updatedb.conf`-style file body, merging into `self`.
    ///
    /// Recognizes `PRUNEPATHS`, `PRUNENAMES`, and `PRUNEFS`. Other keys
    /// (e.g. `PRUNE_BIND_MOUNTS`) are ignored.
    pub fn merge_conf(&mut self, body: &str) {
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let val = val.trim().trim_matches('"');
            let items = val.split_whitespace().map(|s| s.to_string());
            match key {
                "PRUNEPATHS" => self.paths.extend(items),
                "PRUNENAMES" => self.names.extend(items),
                "PRUNEFS" => self.fs_types.extend(items.map(|s| s.to_lowercase())),
                _ => {}
            }
        }
    }
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self::platform_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_conf_parses_keys() {
        let mut c = PruneConfig {
            paths: Default::default(),
            names: Default::default(),
            fs_types: Default::default(),
            cross_mounts: false,
        };
        c.merge_conf(
            r#"
            # comment
            PRUNEPATHS = "/mnt /media"
            PRUNENAMES = ".cache node_modules"
            PRUNEFS = "NFS tmpfs"
            "#,
        );
        assert!(c.should_prune_path("/mnt"));
        assert!(c.should_prune_name("node_modules"));
        assert!(c.fs_types.contains("nfs"));
    }
}
