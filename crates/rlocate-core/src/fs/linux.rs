//! Linux mount enumeration via `/proc/self/mounts`.

use std::path::PathBuf;

/// Read `(mountpoint, fs_type)` pairs from `/proc/self/mounts`.
///
/// Fs-type names are lowercased; mountpoints are octal-unescaped (the kernel
/// encodes spaces and other separators as `\NNN`).
pub fn read_mounts() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(content) = std::fs::read_to_string("/proc/self/mounts") else {
        return out;
    };
    for line in content.lines() {
        // Format: "<dev> <mountpoint> <fstype> <opts> <dump> <pass>"
        let mut fields = line.split(' ');
        let _dev = fields.next();
        let (Some(mp), Some(fstype)) = (fields.next(), fields.next()) else {
            continue;
        };
        out.push((PathBuf::from(unescape_octal(mp)), fstype.to_lowercase()));
    }
    out
}

/// Decode `\NNN` octal escapes used by the kernel in `/proc/self/mounts`.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_octal_spaces() {
        assert_eq!(unescape_octal("/mnt/my\\040drive"), "/mnt/my drive");
        assert_eq!(unescape_octal("/plain/path"), "/plain/path");
    }

    #[test]
    fn reads_root_mount() {
        // On a real Linux host there is always a "/" mount.
        let mounts = read_mounts();
        assert!(mounts.iter().any(|(mp, _)| mp == &PathBuf::from("/")));
    }
}
