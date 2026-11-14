//! Files: open, read and write at an offset, sync, metadata, and the
//! path operations.

use std::io::ErrorKind;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use giong::fs::{self, File, OpenOptions};
use giong::time::timeout;

/// A directory of its own for one test, removed afterwards.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("giong-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic pseudo-random payload.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// How many descriptors of this process refer to `path`. (Counting all
/// descriptors would see those of tests running in parallel.)
fn open_fds(path: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target == path)
        .count()
}

#[test]
fn whole_files_roundtrip() {
    let dir = TempDir::new("whole");
    giong::block_on(async {
        let path = dir.path("a.txt");
        fs::write(&path, "xin chào").await.unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), "xin chào".as_bytes());
        assert_eq!(std::fs::read(&path).unwrap(), "xin chào".as_bytes());

        // Writing again replaces the contents, including a shorter one.
        fs::write(&path, vec![b'x'; 3]).await.unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"xxx");

        // Empty files, and files written by somebody else.
        fs::write(&path, Vec::new()).await.unwrap();
        assert!(fs::read(&path).await.unwrap().is_empty());
        std::fs::write(&path, b"from std").unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), b"from std");
    });
}

#[test]
fn large_files_survive_byte_for_byte() {
    let dir = TempDir::new("large");
    giong::block_on(async {
        let path = dir.path("big.bin");
        let data = payload(8 * 1024 * 1024 + 123, 7);
        fs::write(&path, data.clone()).await.unwrap();
        assert_eq!(fs::metadata(&path).await.unwrap().len(), data.len() as u64);
        assert!(fs::read(&path).await.unwrap() == data);
        assert!(std::fs::read(&path).unwrap() == data);
    });
}

#[test]
fn reads_and_writes_name_their_offset() {
    let dir = TempDir::new("offsets");
    giong::block_on(async {
        let path = dir.path("f");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .await
            .unwrap();

        file.write_all_at("0123456789", 0).await.0.unwrap();
        file.write_all_at(b"abc".to_vec(), 4).await.0.unwrap();
        // Writing past the end leaves a hole of zeros.
        file.write_all_at(&b"end"[..], 20).await.0.unwrap();

        let (res, buf) = file.read_at(Vec::with_capacity(64), 0).await;
        assert_eq!(res.unwrap(), 23);
        assert_eq!(&buf[..10], b"0123abc789");
        assert_eq!(&buf[10..20], &[0u8; 10]);
        assert_eq!(&buf[20..], b"end");

        // A read in the middle, limited by the buffer's capacity. The buffer
        // is reused: its old contents are replaced.
        let mut small = Vec::with_capacity(3);
        small.extend_from_slice(b"zz");
        let (res, small) = file.read_at(small, 4).await;
        assert_eq!((res.unwrap(), &small[..]), (3, &b"abc"[..]));

        // At and past the end of the file.
        let (res, buf) = file.read_at(buf, 23).await;
        assert_eq!((res.unwrap(), buf.len()), (0, 0));
        let (res, _) = file.read_at(buf, 1_000_000).await;
        assert_eq!(res.unwrap(), 0);

        assert_eq!(file.metadata().await.unwrap().len(), 23);
        file.sync_data().await.unwrap();
        file.sync_all().await.unwrap();
        file.close().await.unwrap();
    });
}

#[test]
fn open_options_behave_like_the_standard_library() {
    let dir = TempDir::new("options");
    giong::block_on(async {
        let path = dir.path("f");

        // Missing file.
        let err = File::open(&path).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        let err = File::options().write(true).open(&path).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        // create_new creates once.
        let new = || async {
            File::options()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
        };
        new()
            .await
            .unwrap()
            .write_all_at("first", 0)
            .await
            .0
            .unwrap();
        assert_eq!(new().await.unwrap_err().kind(), ErrorKind::AlreadyExists);

        // create without truncate keeps the contents; File::create empties.
        let file = File::options().write(true).create(true).open(&path).await;
        drop(file.unwrap());
        assert_eq!(fs::read(&path).await.unwrap(), b"first");
        drop(File::create(&path).await.unwrap());
        assert!(fs::read(&path).await.unwrap().is_empty());

        // Append: every write lands at the end, whatever its offset.
        fs::write(&path, "one").await.unwrap();
        let file = File::options().append(true).open(&path).await.unwrap();
        file.write_all_at(",two", 0).await.0.unwrap();
        file.write_all_at(",three", 0).await.0.unwrap();
        drop(file);
        assert_eq!(fs::read(&path).await.unwrap(), b"one,two,three");

        // Truncate.
        let file = File::options().write(true).truncate(true).open(&path).await;
        drop(file.unwrap());
        assert_eq!(fs::metadata(&path).await.unwrap().len(), 0);

        // A file opened for reading cannot be written, and the reverse.
        fs::write(&path, "data").await.unwrap();
        let file = File::open(&path).await.unwrap();
        assert!(file.write_at("x", 0).await.0.is_err());
        let file = File::options().write(true).open(&path).await.unwrap();
        assert!(file.read_at(Vec::with_capacity(4), 0).await.0.is_err());

        // Permission bits of a new file.
        let private = dir.path("private");
        let file = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&private)
            .await
            .unwrap();
        assert_eq!(file.metadata().await.unwrap().mode() & 0o777, 0o600);

        // Combinations that make no sense are refused before any I/O.
        for options in [
            OpenOptions::new(),
            OpenOptions::new().read(true).truncate(true),
            OpenOptions::new().read(true).create(true),
            OpenOptions::new().append(true).truncate(true),
        ] {
            let err = options.open(&path).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{options:?}");
        }
        let err = File::open("nul\0byte").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    });
}

#[test]
fn metadata_describes_files_directories_and_links() {
    let dir = TempDir::new("meta");
    giong::block_on(async {
        let path = dir.path("file");
        fs::write(&path, "12345").await.unwrap();
        let meta = fs::metadata(&path).await.unwrap();
        assert!(meta.is_file() && !meta.is_dir() && !meta.is_symlink());
        assert_eq!((meta.len(), meta.is_empty()), (5, false));
        let age = SystemTime::now().duration_since(meta.modified()).unwrap();
        assert!(age < Duration::from_secs(60), "{age:?}");
        let std_meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.modified(), std_meta.modified().unwrap());

        let meta = fs::metadata(&dir.0).await.unwrap();
        assert!(meta.is_dir() && !meta.is_file());

        let link = dir.path("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(fs::metadata(&link).await.unwrap().is_file());
        assert!(fs::symlink_metadata(&link).await.unwrap().is_symlink());

        let err = fs::metadata(dir.path("missing")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        // Files whose size is not known in advance.
        let status = fs::read("/proc/self/status").await.unwrap();
        assert!(status.starts_with(b"Name:"));
        assert!(status.len() > 100);
    });
}

#[test]
fn files_and_directories_can_be_renamed_and_removed() {
    let dir = TempDir::new("paths");
    giong::block_on(async {
        let (a, b) = (dir.path("a"), dir.path("b"));
        fs::write(&a, "a").await.unwrap();
        fs::write(&b, "b").await.unwrap();

        // Rename replaces the target.
        fs::rename(&a, &b).await.unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&b).await.unwrap(), b"a");
        let err = fs::rename(&a, &b).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        fs::remove_file(&b).await.unwrap();
        assert!(!b.exists());
        let err = fs::remove_file(&b).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        let sub = dir.path("sub");
        fs::create_dir(&sub).await.unwrap();
        assert!(fs::metadata(&sub).await.unwrap().is_dir());
        let err = fs::create_dir(&sub).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);

        // Only empty directories can be removed, and not with remove_file.
        fs::write(sub.join("f"), "x").await.unwrap();
        assert!(fs::remove_dir(&sub).await.is_err());
        assert!(fs::remove_file(&sub).await.is_err());
        fs::remove_file(sub.join("f")).await.unwrap();
        fs::remove_dir(&sub).await.unwrap();
        assert!(!sub.exists());
    });
}

#[test]
fn many_tasks_share_one_file() {
    let dir = TempDir::new("shared");
    giong::block_on(async {
        const BLOCK: usize = 4096;
        const TASKS: usize = 200;
        let path = dir.path("shared");
        let file = Rc::new(
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)
                .await
                .unwrap(),
        );
        // Every task writes its own block, then reads it back.
        let tasks: Vec<_> = (0..TASKS)
            .map(|i| {
                let file = file.clone();
                giong::spawn(async move {
                    let offset = (i * BLOCK) as u64;
                    let block = payload(BLOCK, i as u64);
                    let (res, block) = file.write_all_at(block, offset).await;
                    res.unwrap();
                    let (res, back) = file.read_at(Vec::with_capacity(BLOCK), offset).await;
                    assert_eq!(res.unwrap(), BLOCK);
                    assert!(back == block);
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        let all = fs::read(&path).await.unwrap();
        assert_eq!(all.len(), TASKS * BLOCK);
        for i in 0..TASKS {
            assert!(all[i * BLOCK..(i + 1) * BLOCK] == payload(BLOCK, i as u64)[..]);
        }
    });
}

#[test]
fn closed_and_dropped_files_release_their_descriptors() {
    let dir = TempDir::new("fds");
    let path = dir.path("f");
    giong::block_on(async {
        fs::write(&path, "x").await.unwrap();
        let kept = File::open(&path).await.unwrap();
        for round in 0..200 {
            let file = File::open(&path).await.unwrap();
            if round % 2 == 0 {
                file.close().await.unwrap();
            } else {
                drop(file);
            }
            // A close future that is never polled must not leak either.
            drop(File::open(&path).await.unwrap().close());
        }
        // Yielding lets the driver submit closes but does not wait for them.
        giong::time::timeout(Duration::from_secs(1), async {
            while open_fds(&path) > 1 {
                giong::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("dropped files should close while the runtime is still running");
        assert_eq!(open_fds(&path), 1);
        drop(kept);
    });
    // Dropping a file outside the runtime closes it directly.
    let file = giong::block_on(async { File::open(&path).await.unwrap() });
    assert_eq!(open_fds(&path), 1);
    drop(file);
    assert_eq!(open_fds(&path), 0);
}

#[test]
fn files_dropped_at_shutdown_release_their_descriptors() {
    let dir = TempDir::new("shutdown-fds");
    let path = dir.path("f");
    std::fs::write(&path, "x").unwrap();

    giong::block_on(async {
        let mut files = Vec::new();
        for _ in 0..64 {
            files.push(File::open(&path).await.unwrap());
        }
        // Leave the whole batch of closes for shutdown to drain.
        drop(files);
    });

    assert_eq!(open_fds(&path), 0);
}

#[test]
fn a_read_that_never_completes_can_be_cancelled() {
    // A pipe nobody writes to: the read stays in flight until it is dropped.
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let (reader, writer) = unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    };
    giong::block_on(async move {
        let pipe = File::from_std(reader);
        for _ in 0..20 {
            let read = pipe.read_at(Vec::with_capacity(64 * 1024), 0);
            assert!(timeout(Duration::from_millis(2), read).await.is_err());
        }
        // The pipe still works afterwards. A one-off read on a pipe is not
        // cancel safe: until the kernel has seen the cancellation, a dropped
        // read can still take data. So let the cancellations go out first.
        giong::time::sleep(Duration::from_millis(2)).await;
        std::io::Write::write_all(&mut &writer, b"late").unwrap();
        let (res, buf) = pipe.read_at(Vec::with_capacity(16), 0).await;
        assert_eq!((res.unwrap(), &buf[..]), (4, &b"late"[..]));
    });
}

#[test]
fn a_file_is_closed_only_once_a_blocking_thread_is_done_with_it() {
    // On epoll a read from a terminal runs on a blocking thread and waits
    // there for input. Dropping the read and then the file must not close the
    // descriptor under that thread: the kernel could give its number to the
    // next open, and the thread would read from whatever that is.
    for close in [false, true] {
        let Some((master, slave)) = pseudo_terminal() else {
            eprintln!("skipped: no pseudo-terminals here");
            return;
        };
        closed_after_the_thread(master, slave, close);
    }
}

fn closed_after_the_thread(master: std::fs::File, mut slave: std::fs::File, close: bool) {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    let target = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
    let before = open_fds(&target);
    giong::block_on(async move {
        let file = File::from_std(master);
        let read = file.read_at(Vec::with_capacity(16), 0);
        assert!(timeout(Duration::from_millis(100), read).await.is_err());
        if close {
            file.close().await.unwrap();
        } else {
            drop(file);
        }
        let epoll = giong::driver_kind() == giong::DriverKind::Epoll;
        if epoll {
            // The thread still waits in read(2): the descriptor stays open.
            giong::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(open_fds(&target), before);
        }
        // Once the thread has read something, the descriptor goes. (On
        // io_uring the read was cancelled in the kernel, and the terminal
        // may already be closed, which makes this write fail.)
        let wrote = std::io::Write::write_all(&mut slave, b"x\n");
        if epoll {
            wrote.unwrap();
        }
        let start = Instant::now();
        while open_fds(&target) != before - 1 {
            assert!(start.elapsed() < Duration::from_secs(5), "never closed");
            giong::time::sleep(Duration::from_millis(5)).await;
        }
    });
}

/// A pseudo-terminal's two ends, or `None` where there are none.
fn pseudo_terminal() -> Option<(std::fs::File, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let master = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/ptmx")
        .ok()?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    let mut name = [0 as libc::c_char; 64];
    // SAFETY: `fd` is a terminal master we own; `name` is large enough.
    unsafe {
        if libc::grantpt(fd) != 0
            || libc::unlockpt(fd) != 0
            || libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) != 0
        {
            return None;
        }
    }
    // SAFETY: ptsname_r wrote a NUL-terminated path.
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    let slave = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path.to_str().ok()?)
        .ok()?;
    Some((master, slave))
}

#[test]
fn a_stuck_file_operation_does_not_hold_up_the_others() {
    // On epoll, file operations run on the process's blocking threads. Opening
    // a FIFO for reading blocks until a writer opens it. A burst of such opens
    // queued while one blocking thread was idle used to go to that one thread
    // alone, so the first stuck open held up every other file operation of
    // the process.
    const OPENS: usize = 16;
    let dir = TempDir::new("stuck");
    for i in 0..OPENS {
        let path = std::ffi::CString::new(dir.path(&i.to_string()).to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    let paths: Vec<PathBuf> = (0..OPENS).map(|i| dir.path(&i.to_string())).collect();
    let last = paths[OPENS - 1].clone();
    // A writer for the last FIFO only, after 200 ms.
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        std::fs::OpenOptions::new().write(true).open(last).unwrap()
    });
    let opens = paths.clone();
    giong::Builder::new()
        .driver(giong::DriverKind::Epoll)
        .block_on(async move {
            // One quick operation, so that a blocking thread exists and is idle.
            fs::metadata("/").await.unwrap();
            giong::time::sleep(Duration::from_millis(20)).await;
            let start = Instant::now();
            let mut handles: Vec<_> = opens
                .into_iter()
                .map(|path| giong::spawn(async move { File::open(path).await.map(drop) }))
                .collect();
            let last = handles.pop().unwrap();
            let opened = timeout(Duration::from_secs(5), last).await;
            assert!(
                matches!(opened, Ok(Ok(Ok(())))),
                "the open with a writer did not complete after {:?}: {opened:?}",
                start.elapsed()
            );
            // Let the other opens finish, so that they do not keep blocking
            // threads busy: a FIFO opened for reading and writing has a writer.
            let ends: Vec<std::fs::File> = paths[..OPENS - 1]
                .iter()
                .map(|path| {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(path)
                        .unwrap()
                })
                .collect();
            for handle in handles {
                handle.await.unwrap().unwrap();
            }
            drop(ends);
        })
        .unwrap();
    drop(writer.join().unwrap());
}

#[test]
fn files_work_on_every_worker() {
    let dir = TempDir::new("workers");
    let base = dir.0.clone();
    giong::Builder::new()
        .workers(3)
        .build()
        .unwrap()
        .run(move |worker| {
            let path = base.join(format!("worker-{worker}"));
            async move {
                let data = payload(100_000, worker as u64);
                fs::write(&path, data.clone()).await.unwrap();
                assert!(fs::read(&path).await.unwrap() == data);
            }
        })
        .unwrap();
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 3);
}

#[test]
fn runtime_teardown_with_file_operations_in_flight() {
    let dir = TempDir::new("teardown");
    for round in 0..20 {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (reader, writer) = unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        };
        let path = dir.path(&format!("f{round}"));
        giong::block_on(async move {
            // A read that never completes, and a large write, an open and a
            // sync that may or may not have finished when the runtime stops.
            let pipe = File::from_std(reader);
            giong::spawn(async move {
                let _ = pipe.read_at(Vec::with_capacity(4096), 0).await;
            });
            let file = Rc::new(File::create(&path).await.unwrap());
            let f = file.clone();
            giong::spawn(async move {
                let _ = f.write_all_at(vec![7u8; 4 * 1024 * 1024], 0).await;
                let _ = f.sync_all().await;
            });
            giong::spawn(async move {
                let _ = fs::read("/proc/self/status").await;
            });
            giong::yield_now().await;
        });
        drop(writer);
    }
}

#[test]
fn a_std_file_can_be_adopted_and_knows_its_length() {
    let dir = TempDir::new("from-std");
    let path = dir.path("f");
    std::fs::write(&path, b"std wrote this").unwrap();
    giong::block_on(async {
        let file = File::from_std(std::fs::File::open(&path).unwrap());
        assert_eq!(file.len().unwrap(), 14);
        assert!(!file.is_empty().unwrap());
        assert_eq!(file.metadata().await.unwrap().len(), 14);
        let (res, buf) = file.read_at(Vec::with_capacity(32), 4).await;
        assert_eq!(res.unwrap(), 10);
        assert_eq!(buf, b"wrote this");

        let empty = File::create(dir.path("empty")).await.unwrap();
        assert!(empty.is_empty().unwrap());
    });
}

/// Every entry `read_dir` lists for `dir`, sorted by name.
async fn list(dir: &std::path::Path) -> Vec<fs::DirEntry> {
    let mut entries = Vec::new();
    let mut listing = fs::read_dir(dir).await.unwrap();
    while let Some(entry) = listing.next_entry().await.unwrap() {
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.file_name().cmp(b.file_name()));
    entries
}

#[test]
fn directories_list_like_the_standard_library() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirEntryExt, MetadataExt};

    let dir = TempDir::new("read-dir");
    giong::block_on(async {
        // An empty directory stays empty when asked again.
        let mut empty = fs::read_dir(&dir.0).await.unwrap();
        assert!(empty.next_entry().await.unwrap().is_none());
        assert!(empty.next_entry().await.unwrap().is_none());

        fs::write(dir.path("file"), "12345").await.unwrap();
        fs::create_dir(dir.path("sub")).await.unwrap();
        std::os::unix::fs::symlink(dir.path("file"), dir.path("link")).unwrap();
        std::os::unix::fs::symlink(dir.path("missing"), dir.path("dangling")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.path("socket")).unwrap();
        // A name that is not UTF-8.
        let latin1 = std::ffi::OsStr::from_bytes(b"caf\xe9");
        std::fs::write(dir.0.join(latin1), "").unwrap();

        let ours = list(&dir.0).await;
        let mut theirs: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        theirs.sort_by_key(|entry| entry.file_name());
        let names: Vec<_> = ours
            .iter()
            .map(|entry| entry.file_name().as_bytes())
            .collect();
        assert_eq!(
            names,
            [
                &b"caf\xe9"[..],
                b"dangling",
                b"file",
                b"link",
                b"socket",
                b"sub"
            ]
        );
        assert_eq!(ours.len(), theirs.len());
        for (ours, theirs) in ours.iter().zip(&theirs) {
            assert_eq!(ours.file_name(), theirs.file_name());
            assert_eq!(ours.path(), theirs.path());
            assert_eq!(ours.ino(), theirs.ino());
            // The entry itself: links are not followed.
            let kind = ours.file_type().await.unwrap();
            let std_meta = std::fs::symlink_metadata(theirs.path()).unwrap();
            assert_eq!(
                kind.mode(),
                std_meta.mode() & libc::S_IFMT,
                "{:?}",
                ours.path()
            );
            let std_kind = theirs.file_type().unwrap();
            assert_eq!(
                (kind.is_file(), kind.is_dir(), kind.is_symlink()),
                (std_kind.is_file(), std_kind.is_dir(), std_kind.is_symlink()),
            );
            let meta = ours.metadata().await.unwrap();
            assert_eq!(meta.file_type(), kind);
            assert_eq!(meta.len(), std_meta.len());
        }
        // An entry outlives its listing, and clones share the directory.
        let file = ours[2].clone();
        drop(ours);
        assert_eq!(file.path(), dir.path("file"));
        assert_eq!(file.metadata().await.unwrap().len(), 5);
    });
}

#[test]
fn large_directories_are_read_in_batches() {
    let dir = TempDir::new("read-dir-many");
    // About 80 bytes per entry, so an 8 KiB batch holds about 100 of them.
    let names: std::collections::BTreeSet<String> = (0..3_000)
        .map(|i| format!("entry-{i:05}-{}", "x".repeat(40)))
        .collect();
    for name in &names {
        std::fs::write(dir.path(name), "").unwrap();
    }
    giong::block_on(async {
        let listed: std::collections::BTreeSet<String> = list(&dir.0)
            .await
            .iter()
            .map(|entry| entry.file_name().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(listed, names);

        // Other tasks run between batches.
        let ticks = Rc::new(std::cell::Cell::new(0));
        let ticker = giong::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    ticks.set(ticks.get() + 1);
                    giong::yield_now().await;
                }
            }
        });
        let mut listing = fs::read_dir(&dir.0).await.unwrap();
        let mut count = 0;
        while listing.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        ticker.abort();
        assert_eq!(count, names.len());
        assert!(ticks.get() >= 5, "only {} ticks while listing", ticks.get());

        // A listing whose calls are cut short by a zero timeout loses no
        // entry and still reaches the end.
        let mut listing = fs::read_dir(&dir.0).await.unwrap();
        let (mut seen, mut cancelled) = (std::collections::BTreeSet::new(), 0);
        loop {
            match timeout(Duration::ZERO, listing.next_entry())
                .await
                .map(Result::unwrap)
            {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_str().unwrap().to_owned();
                    assert!(seen.insert(name), "{:?} listed twice", entry.file_name());
                }
                Ok(None) => break,
                Err(_) => cancelled += 1,
            }
        }
        assert_eq!(seen, names);
        assert!(cancelled >= 5, "only {cancelled} calls were cut short");
    });
}

#[test]
fn read_dir_reports_what_it_cannot_list() {
    let dir = TempDir::new("read-dir-errors");
    giong::block_on(async {
        let err = fs::read_dir(dir.path("missing")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        fs::write(dir.path("file"), "x").await.unwrap();
        let err = fs::read_dir(dir.path("file")).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTDIR));

        let err = fs::read_dir("bad\0path").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);

        // An entry removed after it was listed has no metadata.
        let entry = list(&dir.0).await.pop().unwrap();
        fs::remove_file(entry.path()).await.unwrap();
        let err = entry.metadata().await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        // Its type was reported with the name, so it is still known.
        assert!(entry.file_type().await.unwrap().is_file());
    });
}

#[test]
fn dropped_listings_release_their_descriptors() {
    let dir = TempDir::new("read-dir-fds");
    for i in 0..1_000 {
        std::fs::write(dir.path(&format!("{i:04}-{}", "y".repeat(60))), "").unwrap();
    }
    giong::block_on(async {
        for round in 0..20 {
            let mut listing = fs::read_dir(&dir.0).await.unwrap();
            assert_eq!(open_fds(&dir.0), 1);
            // Stop at the start, in the first batch, or in a later one.
            for _ in 0..round * 50 {
                listing.next_entry().await.unwrap().unwrap();
            }
            drop(listing);
        }
        timeout(Duration::from_secs(1), async {
            while open_fds(&dir.0) > 0 {
                giong::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("dropped listings should close their directory");
    });
}

#[test]
fn allocate_reserves_space_without_touching_data() {
    use std::os::unix::fs::MetadataExt;

    let dir = TempDir::new("allocate");
    let path = dir.path("f");
    giong::block_on(async {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .await
            .unwrap();
        match file.allocate(0, 1 << 20).await {
            Err(err) if err.kind() == ErrorKind::Unsupported => {
                eprintln!("skipped: this filesystem has no fallocate ({err})");
                return;
            }
            res => res.unwrap(),
        }
        // The file grows, the space is reserved, and the new bytes are zeros.
        assert_eq!(file.len().unwrap(), 1 << 20);
        assert!(std::fs::metadata(&path).unwrap().blocks() * 512 >= 1 << 20);
        let data = fs::read(&path).await.unwrap();
        assert_eq!(data.len(), 1 << 20);
        assert!(data.iter().all(|&b| b == 0));

        // Inside the file: data stays and the length does not change.
        let pattern = payload(8192, 7);
        file.write_all_at(pattern.clone(), 4096).await.0.unwrap();
        file.allocate(0, 16384).await.unwrap();
        assert_eq!(file.len().unwrap(), 1 << 20);
        let (res, buf) = file.read_at(Vec::with_capacity(8192), 4096).await;
        assert_eq!(res.unwrap(), 8192);
        assert_eq!(buf, pattern);

        // Past the end, with a gap: the file grows to the end of the range.
        file.allocate(2 << 20, 4096).await.unwrap();
        assert_eq!(file.len().unwrap(), (2 << 20) + 4096);

        // The kernel's own errors come back as they are.
        let err = file.allocate(0, 0).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        let read_only = File::open(&path).await.unwrap();
        let err = read_only.allocate(0, 1).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    });
}

#[test]
fn allocations_dropped_mid_flight_are_cleaned_up() {
    let dir = TempDir::new("allocate-cancel");
    let path = dir.path("f");
    giong::block_on(async {
        let file = File::create(&path).await.unwrap();
        // Each call is submitted on its first poll and dropped right after.
        let mut abandoned = 0;
        for i in 0..50u64 {
            match timeout(Duration::ZERO, file.allocate(i << 16, 1 << 16)).await {
                Err(_) => abandoned += 1,
                Ok(Err(err)) if err.kind() == ErrorKind::Unsupported => {
                    eprintln!("skipped: this filesystem has no fallocate ({err})");
                    return;
                }
                Ok(res) => res.unwrap(),
            }
        }
        assert!(abandoned > 0, "expected some allocations to be in flight");
        timeout(Duration::from_secs(5), async {
            while giong::stats().tracked_ops > 0 {
                giong::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("abandoned allocations should complete and be forgotten");
        // Whatever ran left a consistent file, and the next call works.
        assert!(file.len().unwrap() <= 50 << 16);
        file.allocate(0, 50 << 16).await.unwrap();
        assert_eq!(file.len().unwrap(), 50 << 16);
        file.close().await.unwrap();
    });
}
