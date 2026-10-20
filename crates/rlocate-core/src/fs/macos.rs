//! macOS mount enumeration via `getmntinfo(3)`.

use std::ffi::CStr;
use std::path::PathBuf;

/// Read `(mountpoint, fs_type)` pairs for all mounted filesystems.
///
/// Uses `getmntinfo` with `MNT_NOWAIT` (does not block on stat-ing remote
/// mounts). The returned buffer is owned by the C library and must not be
/// freed. Fs-type names (`f_fstypename`, e.g. `apfs`, `nfs`, `smbfs`) are
/// lowercased.
pub fn read_mounts() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    // SAFETY: getmntinfo fills `mntbuf` with a pointer to a static array of
    // `count` statfs structs owned by libc; we only read from it.
    unsafe {
        let mut mntbuf: *mut libc::statfs = std::ptr::null_mut();
        let count = libc::getmntinfo(&mut mntbuf, libc::MNT_NOWAIT);
        if count <= 0 || mntbuf.is_null() {
            return out;
        }
        let entries = std::slice::from_raw_parts(mntbuf, count as usize);
        for fs in entries {
            let fstype = CStr::from_ptr(fs.f_fstypename.as_ptr())
                .to_string_lossy()
                .to_lowercase();
            let mnt = CStr::from_ptr(fs.f_mntonname.as_ptr())
                .to_string_lossy()
                .into_owned();
            out.push((PathBuf::from(mnt), fstype));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_root_apfs_mount() {
        let mounts = read_mounts();
        // Every Mac has a "/" mount.
        assert!(mounts.iter().any(|(mp, _)| mp == &PathBuf::from("/")));
    }
}
