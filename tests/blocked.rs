//! What happens where io_uring is blocked, as in a Docker container with the
//! default seccomp profile: each test runs itself again in a child process
//! that installs a seccomp filter making `io_uring_setup` fail with `EPERM`
//! (or, for a kernel that refuses buffer rings, makes registering one fail
//! with `EINVAL`, or sets the locked-memory limit that io_uring's memory
//! counts against to nothing).
//!
//! By default giong then runs on its epoll driver, says so once on stderr
//! and in `driver_choice`; a runtime that insists on io_uring fails with an
//! error that says why and what to do.

use std::process::Command;

use giong::error::Code;
use giong::{ChosenBy, DriverKind};

const CHILD: &str = "GIONG_TEST_BLOCKED_CHILD";

/// Runs the test `name` again in a child process, which blocks io_uring
/// itself, and returns what it printed on stdout and stderr. Panics if the
/// child failed.
fn run_blocked_child_with_stderr(name: &str) -> (String, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .env_remove("GIONG_DRIVER")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stdout}{stderr}");
    (stdout, stderr)
}

fn run_blocked_child(name: &str) -> String {
    run_blocked_child_with_stderr(name).0
}

/// The lines giong wrote to stderr when it fell back to epoll.
fn fallback_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("giong: running on the epoll driver"))
        .collect()
}

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

#[test]
fn a_runtime_that_requires_io_uring_says_why_it_cannot_start() {
    if in_child() {
        block_io_uring_setup();
        let rt = giong::Builder::new()
            .workers(1)
            .driver(DriverKind::IoUring)
            .build()
            .unwrap();
        let err = rt.run(|_| async {}).unwrap_err();
        println!("run: {err}");
        println!("code: {:?}", giong::error::code(&err));
        let err = giong::Builder::new()
            .driver(DriverKind::IoUring)
            .block_on(async {})
            .unwrap_err();
        println!("block_on: {err}");
        // The environment variable asks for the same thing.
        std::env::set_var("GIONG_DRIVER", "io_uring");
        let err = giong::Builder::new().block_on(async {}).unwrap_err();
        println!("env: {err}");
        return;
    }

    let stdout = run_blocked_child("a_runtime_that_requires_io_uring_says_why_it_cannot_start");
    for call in ["run", "block_on", "env"] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(&format!("{call}: ")))
            .unwrap_or_else(|| panic!("no {call} line in {stdout}"));
        assert!(
            line.starts_with(&format!("{call}: GIONG-E0001: ")),
            "{line}"
        );
        assert!(line.contains("giong needs io_uring"), "{line}");
        assert!(line.contains("seccomp"), "{line}");
        assert!(line.contains("epoll driver"), "{line}");
        assert!(line.contains("docs/DEPLOYMENT.md"), "{line}");
        assert!(line.contains("docs/errors/E0001.md"), "{line}");
    }
    assert!(
        stdout.contains("code: Some(IoUringUnavailable)"),
        "{stdout}"
    );
}

#[test]
fn the_default_runtime_falls_back_to_epoll_and_works() {
    if in_child() {
        block_io_uring_setup();
        let rt = giong::Builder::new().workers(2).build().unwrap();
        let handle = rt.handle();
        rt.run(move |worker| {
            let handle = handle.clone();
            async move { worked_on_epoll(worker, handle).await }
        })
        .unwrap();
        return;
    }

    let (stdout, stderr) =
        run_blocked_child_with_stderr("the_default_runtime_falls_back_to_epoll_and_works");
    assert!(stdout.contains("worked on epoll"), "{stdout}");
    // Two workers fell back; the process says so once.
    let lines = fallback_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("GIONG-E0001"), "{}", lines[0]);
    assert!(lines[0].contains("GIONG_DRIVER"), "{}", lines[0]);
}

/// What each worker of the fallen-back runtime checks and does.
async fn worked_on_epoll(worker: usize, handle: giong::Handle) {
    assert_eq!(giong::driver_kind(), DriverKind::Epoll);
    let choice = giong::driver_choice();
    assert_eq!(choice.kind, DriverKind::Epoll);
    assert_eq!(choice.chosen_by, ChosenBy::Automatic);
    assert_eq!(choice.io_uring_error_code, Some(Code::IoUringUnavailable));
    assert!(choice.kernel_workarounds.is_empty());
    let error = choice.io_uring_error.as_deref().unwrap();
    assert!(
        error.starts_with("GIONG-E0001: giong needs io_uring"),
        "{error}"
    );
    assert_eq!(handle.driver_choice(worker), Some(choice.clone()));
    let line = choice.to_string();
    assert!(
        line.starts_with(
            "epoll (chosen automatically): io_uring cannot be used here: GIONG-E0001: "
        ),
        "{line}"
    );
    if worker != 0 {
        return;
    }
    // A connection, a file and a timer: the three kinds of work the
    // epoll driver does differently.
    let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = giong::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let data = stream.recv().await.unwrap();
        let reply = data.to_vec();
        drop(data);
        stream.write_all(reply).await.0.unwrap();
    });
    let client = giong::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(&b"xin chao"[..]).await.0.unwrap();
    let echo = client.recv().await.unwrap();
    assert_eq!(&echo[..], b"xin chao");
    drop(echo);
    server.await.unwrap();

    let path = std::env::temp_dir().join(format!("giong-blocked-{}", std::process::id()));
    giong::fs::write(&path, "epoll").await.unwrap();
    assert_eq!(giong::fs::read(&path).await.unwrap(), b"epoll");
    giong::fs::remove_file(&path).await.unwrap();

    giong::time::sleep(std::time::Duration::from_millis(5)).await;
    println!("worked on epoll");
}

/// A kernel from 6.0 on that will not register a buffer ring is named in
/// the error, not called too old, and giong falls back to epoll saying why.
#[test]
fn a_kernel_that_refuses_buffer_rings_is_named_not_called_old() {
    if in_child() {
        if !io_uring_setup_works() {
            println!("skipped: io_uring is not available here");
            return;
        }
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        println!("release: {}", release.trim());
        block_seccomp(
            libc::SYS_io_uring_register,
            Some(IORING_REGISTER_PBUF_RING),
            libc::EINVAL,
        );
        let choice = giong::Builder::new()
            .block_on(async { giong::driver_choice() })
            .unwrap();
        println!("kind: {}", choice.kind);
        println!("code: {:?}", choice.io_uring_error_code);
        println!("error: {}", choice.io_uring_error.unwrap_or_default());
        let err = giong::Builder::new()
            .driver(DriverKind::IoUring)
            .block_on(async {})
            .unwrap_err();
        println!("named: {err}");
        println!("named code: {:?}", giong::error::code(&err));
        // A second automatic runtime falls back as well, silently.
        giong::Builder::new().block_on(async {}).unwrap();
        return;
    }

    let (stdout, stderr) =
        run_blocked_child_with_stderr("a_kernel_that_refuses_buffer_rings_is_named_not_called_old");
    if stdout.contains("skipped:") {
        return;
    }
    let release = stdout
        .lines()
        .find_map(|l| l.strip_prefix("release: "))
        .unwrap();
    let major: u32 = release.split('.').next().unwrap().parse().unwrap();
    let old = major < 6;
    let (code, number) = if old {
        ("KernelTooOld", "GIONG-E0002")
    } else {
        ("BufferRingRefused", "GIONG-E0009")
    };
    assert!(stdout.contains("kind: epoll"), "{stdout}");
    assert!(stdout.contains(&format!("code: Some({code})")), "{stdout}");
    assert!(
        stdout.contains(&format!("named code: Some({code})")),
        "{stdout}"
    );
    for prefix in ["error: ", "named: "] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} line in {stdout}"));
        assert!(line.contains(number), "{line}");
        assert!(line.contains(release), "{line}");
        assert!(line.contains("Invalid argument"), "{line}");
        if !old {
            assert!(!line.contains("too old"), "{line}");
            assert!(line.contains("uname -r"), "{line}");
        }
    }
    let lines = fallback_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains(number), "{}", lines[0]);
}

/// Since Linux 6.14 every io_uring counts against the locked-memory limit
/// (RLIMIT_MEMLOCK), so a service under a low limit fails to set up its
/// rings with a bare ENOMEM. The child sets the limit to nothing and drops
/// CAP_IPC_LOCK, which would lift it: workers then run on epoll, and the
/// reason names the limit instead of "Cannot allocate memory" alone.
#[test]
fn a_used_up_locked_memory_limit_is_named_and_falls_back_to_epoll() {
    if in_child() {
        if !io_uring_setup_works() {
            println!("skipped: io_uring is not available here");
            return;
        }
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        println!("release: {}", release.trim());
        lock_no_memory();
        let choice = giong::Builder::new()
            .workers(2)
            .block_on(async { giong::driver_choice() })
            .unwrap();
        println!("kind: {}", choice.kind);
        println!("code: {:?}", choice.io_uring_error_code);
        println!("error: {}", choice.io_uring_error.unwrap_or_default());
        match giong::Builder::new()
            .driver(DriverKind::IoUring)
            .block_on(async {})
        {
            Ok(()) => println!("named: ok"),
            Err(err) => {
                println!("named: {err}");
                println!("named code: {:?}", giong::error::code(&err));
            }
        }
        return;
    }

    let (stdout, stderr) = run_blocked_child_with_stderr(
        "a_used_up_locked_memory_limit_is_named_and_falls_back_to_epoll",
    );
    if stdout.contains("skipped:") {
        return;
    }
    let release = stdout
        .lines()
        .find_map(|l| l.strip_prefix("release: "))
        .unwrap();
    let mut version = release.split(|c: char| !c.is_ascii_digit());
    let version: (u32, u32) = (
        version.next().unwrap().parse().unwrap(),
        version.next().unwrap().parse().unwrap(),
    );
    if stdout.contains("kind: io_uring") {
        // Kernels before 6.14 do not count the rings, and the limit does
        // not matter.
        assert!(version < (6, 14), "{stdout}");
        assert!(stdout.contains("named: ok"), "{stdout}");
        return;
    }
    assert!(stdout.contains("kind: epoll"), "{stdout}");
    assert!(stdout.contains("code: Some(LockedMemoryLimit)"), "{stdout}");
    assert!(
        stdout.contains("named code: Some(LockedMemoryLimit)"),
        "{stdout}"
    );
    for prefix in ["error: ", "named: "] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} line in {stdout}"));
        assert!(line.contains("GIONG-E0010"), "{line}");
        assert!(line.contains("(RLIMIT_MEMLOCK, 0 bytes)"), "{line}");
        assert!(line.contains("Cannot allocate memory"), "{line}");
        assert!(line.contains("LimitMEMLOCK=infinity"), "{line}");
    }
    // Two workers fell back; the process says so once.
    let lines = fallback_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("GIONG-E0010"), "{}", lines[0]);
}

/// Sets this process's locked-memory limit to nothing and takes
/// `CAP_IPC_LOCK`, which lifts the limit, from this thread (and so from the
/// threads it starts afterwards), as for a service that is not root.
fn lock_no_memory() {
    /// `struct __user_cap_header_struct` and `struct __user_cap_data_struct`.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_IPC_LOCK: u32 = 14;

    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: plain system calls, each given structs of the types it
    // expects (version 3 of the capability structs takes two of data).
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_MEMLOCK, &none), 0);
        let res = libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr());
        assert_eq!(res, 0, "capget: {}", std::io::Error::last_os_error());
        data[0].effective &= !(1 << CAP_IPC_LOCK);
        data[0].permitted &= !(1 << CAP_IPC_LOCK);
        data[0].inheritable &= !(1 << CAP_IPC_LOCK);
        let res = libc::syscall(libc::SYS_capset, &mut header, data.as_ptr());
        assert_eq!(res, 0, "capset: {}", std::io::Error::last_os_error());
    }
}

/// `IORING_REGISTER_PBUF_RING`.
const IORING_REGISTER_PBUF_RING: u32 = 22;

/// `true` if this process may set up an io_uring at all.
fn io_uring_setup_works() -> bool {
    giong::Builder::new()
        .driver(DriverKind::IoUring)
        .block_on(async {})
        .is_ok()
}

/// Makes `io_uring_setup` fail with `EPERM` in every thread of this process,
/// the way Docker's default seccomp profile does.
fn block_io_uring_setup() {
    block_seccomp(libc::SYS_io_uring_setup, None, libc::EPERM);
}

/// Makes system call `nr` fail with `errno` in every thread of this process,
/// or only when its second argument is `arg1`.
fn block_seccomp(nr: libc::c_long, arg1: Option<u32>, errno: i32) {
    const LOAD_SYSCALL_NR: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JUMP_IF_EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    // The syscall number is the first field of `struct seccomp_data`; the
    // arguments start at byte 16, eight bytes each (the low half first on
    // x86-64).
    let mut filter = vec![
        op(LOAD_SYSCALL_NR, 0, 0, 0),
        op(JUMP_IF_EQUAL, 0, 3, nr as u32),
    ];
    match arg1 {
        Some(value) => filter.extend([
            op(LOAD_SYSCALL_NR, 0, 0, 16 + 8),
            op(JUMP_IF_EQUAL, 0, 1, value),
        ]),
        // Two no-op jumps keep the offsets the same.
        None => filter.extend([op(JUMP_IF_EQUAL, 0, 0, 0), op(JUMP_IF_EQUAL, 0, 0, 0)]),
    }
    filter.extend([
        op(RETURN, 0, 0, libc::SECCOMP_RET_ERRNO | errno as u32),
        op(RETURN, 0, 0, libc::SECCOMP_RET_ALLOW),
    ]);
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: plain system calls; `prog` and `filter` outlive them, and the
    // kernel copies the filter.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        let res = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &prog as *const libc::sock_fprog,
        );
        assert_eq!(res, 0, "seccomp: {}", std::io::Error::last_os_error());
    }
}
