//! Files: open, read and write at an offset, sync, metadata, and the
//! path operations.

use std::io::ErrorKind;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

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
