//! What happens where io_uring is blocked, as in a Docker container with the
//! default seccomp profile: each test runs itself again in a child process
//! that installs a seccomp filter making `io_uring_setup` fail with `EPERM`.
//!
//! By default giong then runs on its epoll driver; a runtime that insists on
//! io_uring fails with an error that says why and what to do.

use std::process::Command;

use giong::DriverKind;

const CHILD: &str = "GIONG_TEST_BLOCKED_CHILD";

/// Runs the test `name` again in a child process with io_uring blocked, and
/// returns what it printed. Panics if the child failed.
fn run_blocked_child(name: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .env_remove("GIONG_DRIVER")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
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
        rt.run(|worker| async move {
            assert_eq!(giong::driver_kind(), DriverKind::Epoll);
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
        })
        .unwrap();
        return;
    }

    let stdout = run_blocked_child("the_default_runtime_falls_back_to_epoll_and_works");
    assert!(stdout.contains("worked on epoll"), "{stdout}");
}

/// Makes `io_uring_setup` fail with `EPERM` in every thread of this process,
/// the way Docker's default seccomp profile does.
fn block_io_uring_setup() {
    const LOAD_SYSCALL_NR: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JUMP_IF_EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    let filter = [
        // The syscall number is the first field of `struct seccomp_data`.
        libc::sock_filter {
            code: LOAD_SYSCALL_NR,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: JUMP_IF_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_io_uring_setup as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
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
