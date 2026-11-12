//! What a user sees when io_uring is blocked, as in a Docker container with
//! the default seccomp profile: the test runs itself again in a child process
//! that installs a seccomp filter making `io_uring_setup` fail with `EPERM`.

use std::process::Command;

const CHILD: &str = "GIONG_TEST_BLOCKED_CHILD";

#[test]
fn a_blocked_io_uring_is_explained() {
    if std::env::var_os(CHILD).is_some() {
        block_io_uring_setup();
        let rt = giong::Builder::new().workers(1).build().unwrap();
        let err = rt.run(|_| async {}).unwrap_err();
        println!("run: {err}");
        let err = giong::Builder::new().block_on(async {}).unwrap_err();
        println!("block_on: {err}");
        return;
    }

    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "a_blocked_io_uring_is_explained", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for call in ["run", "block_on"] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(&format!("{call}: ")))
            .unwrap_or_else(|| panic!("no {call} line in {stdout}"));
        assert!(line.contains("giong needs io_uring"), "{line}");
        assert!(line.contains("seccomp"), "{line}");
        assert!(line.contains("docs/DEPLOYMENT.md"), "{line}");
    }
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
