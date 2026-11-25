//! Folders under the temporary directory that no other user can enter.
//!
//! On Linux the temporary directory is usually `/tmp`, shared by every user of
//! the machine. A folder there with a name another user can guess (a process
//! id, a commit) can be made first by that user, between giong removing an old
//! one and making it again: giong would then write into a folder the other
//! user can change, and run what is in it (a downloaded binary, a build
//! script). So giong only works in folders it made itself, with mode 0700:
//!
//! - [`fresh`]: a new folder with a name no one chose before, for one run.
//! - [`mine`]: a stable path inside `giong-<uid>`, a folder of this user's that
//!   only they can enter, for paths that must stay the same between runs.
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

/// `<temp>/giong-<uid>/<name>`: a path that stays the same between runs,
/// inside a folder only this user can enter. The folder is made if it is not
/// there; `name` is not.
pub fn mine(name: &str) -> io::Result<PathBuf> {
    Ok(user_dir(&std::env::temp_dir())?.join(name))
}

#[cfg(unix)]
fn user_dir(base: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // This user's id, without unsafe: the owner of a folder just made.
    let probe = fresh_in(base, "giong-uid")?;
    let uid = fs::symlink_metadata(&probe).map(|m| m.uid());
    let _ = fs::remove_dir(&probe);
    let uid = uid?;
    let dir = base.join(format!("giong-{uid}"));
    match make_private(&dir) {
        Ok(()) => return Ok(dir),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    // Not followed: a link there could point anywhere.
    let m = fs::symlink_metadata(&dir)?;
    if m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0 {
        Ok(dir)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a folder that only this user can enter; remove it, or set TMPDIR \
                 to a folder of your own",
                dir.display()
            ),
        ))
    }
}

#[cfg(not(unix))]
fn user_dir(base: &Path) -> io::Result<PathBuf> {
    let dir = base.join("giong");
    fs::create_dir_all(&dir)?;
    Ok(dir)
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

    #[test]
    fn the_user_folder_is_private_and_kept() {
        let a = mine("x").unwrap();
        let b = mine("x").unwrap();
        assert_eq!(a, b);
        let dir = a.parent().unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_user_folder_others_can_enter_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Its own base folder, so the real giong-<uid> is not touched.
        let base = fresh("giong-tmp-open").unwrap();
        let uid = fs::metadata(&base).unwrap().uid();
        let open = base.join(format!("giong-{uid}"));
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let refused = |base: &Path| user_dir(base).unwrap_err().kind();
        assert_eq!(refused(&base), io::ErrorKind::PermissionDenied);
        // A link to a private folder is refused too.
        fs::remove_dir(&open).unwrap();
        let target = fresh_in(&base, "target").unwrap();
        std::os::unix::fs::symlink(&target, &open).unwrap();
        assert_eq!(refused(&base), io::ErrorKind::PermissionDenied);
        // Once it is gone, it is made private.
        fs::remove_file(&open).unwrap();
        assert_eq!(user_dir(&base).unwrap(), open);
        fs::remove_dir_all(&base).unwrap();
    }
}
