//! Saved-remote configuration for the client.
//!
//! An INI-style file lists named remotes so the client can query them without
//! repeating `--host`/`--token`. Each `[name]` section has a `host` and a
//! `token`:
//!
//! ```ini
//! # ~/.config/rlocate/remotes.conf   (chmod 600 — it holds tokens)
//! [plex]
//! host = plex.local:9999
//! token = s3cret
//!
//! [proxmox]
//! host = 10.0.0.5:9999
//! token = hunter2
//! ```

use std::io;
use std::path::{Path, PathBuf};

/// A saved remote server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub host: String,
    pub token: String,
}

/// Default config path: `$XDG_CONFIG_HOME/rlocate/remotes.conf`, else
/// `~/.config/rlocate/remotes.conf`.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("rlocate").join("remotes.conf"))
}

/// Load and parse the remotes file at `path`. A missing file yields an empty
/// list (not an error).
pub fn load(path: &Path) -> io::Result<Vec<Remote>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(parse(&body)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Parse the INI-style body. Sections with no `host` are skipped.
pub fn parse(body: &str) -> Vec<Remote> {
    let mut remotes = Vec::new();
    let mut current: Option<Remote> = None;

    let flush = |cur: &mut Option<Remote>, out: &mut Vec<Remote>| {
        if let Some(r) = cur.take() {
            if !r.host.is_empty() {
                out.push(r);
            }
        }
    };

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&mut current, &mut remotes);
            current = Some(Remote {
                name: name.trim().to_string(),
                host: String::new(),
                token: String::new(),
            });
        } else if let Some((key, val)) = line.split_once('=') {
            if let Some(r) = current.as_mut() {
                let val = val.trim().to_string();
                match key.trim() {
                    "host" => r.host = val,
                    "token" => r.token = val,
                    _ => {}
                }
            }
        }
    }
    flush(&mut current, &mut remotes);
    remotes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections() {
        let body = r#"
            # my remotes
            [plex]
            host = plex.local:9999
            token = s3cret

            [proxmox]
            host = 10.0.0.5:9999
            token = hunter2
        "#;
        let r = parse(body);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            Remote {
                name: "plex".into(),
                host: "plex.local:9999".into(),
                token: "s3cret".into()
            }
        );
        assert_eq!(r[1].name, "proxmox");
        assert_eq!(r[1].token, "hunter2");
    }

    #[test]
    fn skips_sections_without_host() {
        let body = "[empty]\ntoken = x\n[ok]\nhost = h:1\n";
        let r = parse(body);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "ok");
    }
}
