//! giong's own files in a project's `.giong/` folder, never through a link.
//!
//! The folder sits in a repository, and git checks out a committed symbolic
//! link as a link. Followed, `.giong/runner.log -> ~/.bashrc` would let a
//! hostile repository have `giong dev --background` truncate the target, and
//! `.giong/dev.log -> ~/.ssh/id_rsa` would let `giong dev logs` hand the key
//! to an agent. So `.giong` must be a real folder, and every file in it is
//! opened without following a link in its last component (O_NOFOLLOW on
//! Unix; on Windows the link itself is opened, and refused).
//!
//! Every read and write of a `.giong` file goes through here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// `root/.giong`, unchecked.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".giong")
}

/// `root/.giong`, made if missing; refused when it is a link or not a folder.
pub fn ensure(root: &Path) -> io::Result<PathBuf> {
    let dir = dir(root);
    fs::create_dir_all(root)?;
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_dir(&dir)?;
    Ok(dir)
}

/// Refuses a `.giong` that is a link, or something other than a folder. A
/// missing one is fine: there is nothing to follow.
pub fn check_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => Err(link(dir)),
        Ok(m) if !m.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a folder", dir.display()),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn link(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} is a symbolic link, and giong does not follow links in .giong \
             (remove it; giong recreates its files)",
            path.display()
        ),
    )
}

/// Refuses a link at any folder from `path`'s parent up to and including
/// the `.giong` folder it is in (just the parent for a path outside one).
fn check_folders(path: &Path) -> io::Result<()> {
    let inside = path
        .ancestors()
        .skip(1)
        .any(|a| a.file_name() == Some(".giong".as_ref()));
    for folder in path.ancestors().skip(1) {
        check_dir(folder)?;
        if !inside || folder.file_name() == Some(".giong".as_ref()) {
            break;
        }
    }
    Ok(())
}

/// Opens `path` (a file in a `.giong` folder) with `options`, refusing a
/// link at `path` or at a folder between it and `.giong`.
fn open(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    check_folders(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        options.open(path).map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                link(path)
            } else {
                e
            }
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Opens a link itself rather than what it names, so a link that
        // appears after the check below is not followed either.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(link(path));
        }
        let file = options.open(path)?;
        if file.metadata()?.file_type().is_symlink() {
            return Err(link(path));
        }
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(link(path));
        }
        options.open(path)
    }
}

/// Whether a `.giong` file is there (a link counts: it is not followed).
pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Opens `path` for reading.
pub fn open_read(path: &Path) -> io::Result<File> {
    open(path, OpenOptions::new().read(true))
}

pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path, OpenOptions::new().read(true))?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_to_string(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    open(path, OpenOptions::new().read(true))?.read_to_string(&mut text)?;
    Ok(text)
}

/// Opens `path` for writing, emptied (made if missing).
pub fn create(path: &Path) -> io::Result<File> {
    open(
        path,
        OpenOptions::new().write(true).create(true).truncate(true),
    )
}

/// Replaces `path` in one step: written aside to a new file, then renamed
/// over it (a rename replaces a link; it does not follow it).
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let written = open(&tmp, OpenOptions::new().write(true).create_new(true))
        .and_then(|mut f| f.write_all(bytes));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("giong-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn committed_links_are_not_followed() {
        let root = scratch("links");
        let secret = root.join("secret");
        fs::write(&secret, "do not touch").unwrap();
        let dir = ensure(&root).unwrap();
        for name in ["dev.log", "runner.log", "dev.json", "processes.json"] {
            symlink(&secret, dir.join(name)).unwrap();
            let path = dir.join(name);
            assert!(read(&path).is_err(), "{name} was read through a link");
            assert!(read_to_string(&path).is_err());
            assert!(create(&path).is_err(), "{name} was written through a link");
            assert!(exists(&path));
        }
        // A predictable aside name is replaced, not followed.
        symlink(
            &secret,
            dir.join(format!("dev.json.{}", std::process::id())),
        )
        .unwrap();
        fs::remove_file(dir.join("dev.json")).unwrap();
        replace(&dir.join("dev.json"), b"{}").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "do not touch");
        assert_eq!(read_to_string(&dir.join("dev.json")).unwrap(), "{}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_state_folder_is_refused() {
        let root = scratch("dir");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        symlink(&elsewhere, project.join(".giong")).unwrap();
        assert!(ensure(&project).is_err());
        assert!(create(&dir(&project).join("dev.log")).is_err());
        assert!(read(&dir(&project).join("dev.log")).is_err());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        // A file further down a linked .giong is refused too.
        fs::create_dir_all(elsewhere.join("bench")).unwrap();
        let nested = dir(&project).join("bench").join("service.log");
        assert!(create(&nested).is_err());
        assert!(read(&nested).is_err());
        assert!(fs::read_dir(elsewhere.join("bench"))
            .unwrap()
            .next()
            .is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
