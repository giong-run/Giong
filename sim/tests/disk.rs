//! Files under disk faults: slow calls, `EIO`, `ENOSPC`, short reads and
//! writes. Every operation either fails with an error or does what it
//! says; data that was written and synced reads back unchanged; nothing
//! hangs and no descriptor is left open (`Report::open`).

use std::path::{Path, PathBuf};

use giong_sim::Faults;

const TASKS: u64 = 6;
const CHUNK: usize = 3000;
const CHUNKS: usize = 8;

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("giong-sim-disk-{}", std::process::id()))
}

fn content(task: u64) -> Vec<u8> {
    (0..CHUNK * CHUNKS)
        .map(|i| (i as u64 * 31 + task * 7) as u8)
        .collect()
}

/// What each task saw: `Ok(true)` written, synced and read back the same;
/// `Ok(false)` some step reported an error; `Err` wrong data (a bug).
async fn one_file(path: PathBuf, task: u64) -> Result<bool, String> {
    let data = content(task);
    let file = match giong::fs::File::create(&path).await {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    for (i, chunk) in data.chunks(CHUNK).enumerate() {
        if file
            .write_all_at(chunk.to_vec(), (i * CHUNK) as u64)
            .await
            .0
            .is_err()
        {
            let _ = file.close().await;
            return Ok(false);
        }
    }
    if file.sync_all().await.is_err() {
        let _ = file.close().await;
        return Ok(false);
    }
    if file.close().await.is_err() {
        return Ok(false);
    }
    // Read back twice: the whole file, then chunk by chunk with `read_at`.
    match giong::fs::read(&path).await {
        Ok(got) if got == data => {}
        Ok(got) => {
            return Err(format!(
                "task {task}: read {} bytes, not the {} written",
                got.len(),
                data.len()
            ))
        }
        Err(_) => return Ok(false),
    }
    let file = match giong::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    let mut got = Vec::new();
    loop {
        let (res, buf) = file
            .read_at(Vec::with_capacity(CHUNK), got.len() as u64)
            .await;
        match res {
            Ok(0) => break,
            Ok(_) => got.extend_from_slice(&buf),
            Err(_) => {
                let _ = file.close().await;
                return Ok(false);
            }
        }
    }
    let _ = file.close().await;
    if got != data {
        return Err(format!("task {task}: read_at gave different bytes"));
    }
    Ok(true)
}

/// Runs the tasks on two workers; returns (completed, failed).
fn scenario(dir: &Path) -> (u64, u64) {
    let (ok, failed) = (
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    );
    let (o, f) = (ok.clone(), failed.clone());
    let dir = dir.to_path_buf();
    giong::Builder::new()
        .workers(2)
        .build()
        .unwrap()
        .run(move |index| {
            let (o, f, dir) = (o.clone(), f.clone(), dir.clone());
            async move {
                if index != 0 {
                    giong::shutdown_signal().await;
                    return;
                }
                let tasks: Vec<_> = (0..TASKS)
                    .map(|t| giong::spawn(one_file(dir.join(format!("f{t}")), t)))
                    .collect();
                for t in tasks {
                    match t.await.unwrap() {
                        Ok(true) => o.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                        Ok(false) => f.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                        Err(e) => panic!("{e}"),
                    };
                }
                giong::handle().shutdown();
            }
        })
        .unwrap();
    (
        ok.load(std::sync::atomic::Ordering::SeqCst),
        failed.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[test]
fn slow_disk_and_short_transfers_never_show_as_errors() {
    let dir = dir().join("slow");
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.to_str().unwrap().to_string();
    let faults = Faults {
        short_io: 0.3,
        ..Faults::none()
    }
    .disk(prefix, 0.0, 0.0, 0.05);
    for r in giong_sim::sweep(0..30, faults, |_| {
        assert_eq!(scenario(&dir), (TASKS, 0));
    }) {
        assert!(
            r.injected.get("short-file-io").copied().unwrap_or(0) > 0,
            "{r:?}"
        );
        assert!(
            r.open.is_empty(),
            "seed {}: left open: {:?}",
            r.seed,
            r.open
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failing_disk_gives_errors_never_wrong_data() {
    let dir = dir().join("failing");
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.to_str().unwrap().to_string();
    let seeds: u64 = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let faults = Faults {
        short_io: 0.2,
        ..Faults::none()
    }
    .disk(prefix, 0.02, 0.02, 0.02);
    let (mut ok, mut failed) = (0, 0);
    for r in giong_sim::sweep(0..seeds, faults, |_| {
        let (o, f) = scenario(&dir);
        assert_eq!(o + f, TASKS);
    }) {
        assert!(
            r.open.is_empty(),
            "seed {}: left open: {:?}",
            r.seed,
            r.open
        );
        let errors = r.injected.get("disk-eio").copied().unwrap_or(0)
            + r.injected.get("disk-enospc").copied().unwrap_or(0);
        if errors > 0 {
            failed += 1;
        } else {
            ok += 1;
        }
    }
    assert!(
        failed > 0 && ok > 0,
        "faults never or always fired: {ok} clean runs, {failed} with errors"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
