//! Folders under the temporary directory that no other user can enter.
//!
//! On Linux the temporary directory is usually `/tmp`, shared by every user of
//! the machine. A folder there with a name another user can guess (a process
//! id, a commit) can be made first by that user, between giong removing an old
//! one and making it again: giong would then write into a folder the other
//! user can change, and run what is in it (a downloaded binary, a build
//! script). So giong only works in folders it made itself, with mode 0700:
//! [`fresh`] makes a new folder with a name no one chose before, for one run.
//!
//! Cargo never runs below the temporary directory, even in such a folder: it
//! reads `.cargo/config.toml` from every folder above the one it runs in, and
//! any user can make `/tmp/.cargo/config.toml` (a `build.rustc-wrapper` there
//! runs as whoever builds below `/tmp`). Builds that need a path that stays
//! the same between runs go under `~/.giong` instead.
//!
//! On macOS and Windows the temporary directory is the user's own already;
//! the same rules hold there and cost nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Makes `dir`, and only `dir`: fails if anything is already there.
fn make_private(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(dir)
    }
}

/// A new, empty folder `<temp>/<prefix>-<unique>` that this process made,
/// with mode 0700. The caller removes it.
pub fn fresh(prefix: &str) -> io::Result<PathBuf> {
    fresh_in(&std::env::temp_dir(), prefix)
}

fn fresh_in(base: &Path, prefix: &str) -> io::Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut last = None;
    for _ in 0..64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let seed = format!(
            "{}-{nanos}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        );
        let unique = &crate::sha256::hex(seed.as_bytes())[..16];
        let dir = base.join(format!("{prefix}-{unique}"));
        match make_private(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free name")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_folders_are_new_and_private() {
        let a = fresh("giong-tmp-test").unwrap();
        let b = fresh("giong-tmp-test").unwrap();
        assert_ne!(a, b);
        assert!(fs::read_dir(&a).unwrap().next().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&a).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        }
        fs::remove_dir(a).unwrap();
        fs::remove_dir(b).unwrap();
    }
}
