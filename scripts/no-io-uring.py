#!/usr/bin/env python3
"""Runs a command with io_uring blocked, the way Docker's default seccomp
profile blocks it: io_uring_setup, io_uring_enter and io_uring_register fail
with EPERM in the command and everything it starts.

    scripts/no-io-uring.py cargo test --workspace

giong then picks its epoll driver by itself; scripts/check.sh seccomp runs
the whole test suite like this. Needs Linux and Python 3; no privileges (the
filter is installed with no_new_privs, which any process may set).
"""

import ctypes
import os
import struct
import sys

PR_SET_NO_NEW_PRIVS = 38
PR_SET_SECCOMP = 22
SECCOMP_MODE_FILTER = 2

BPF_LD_W_ABS = 0x20  # BPF_LD | BPF_W | BPF_ABS
BPF_JEQ_K = 0x15  # BPF_JMP | BPF_JEQ | BPF_K
BPF_RET_K = 0x06  # BPF_RET | BPF_K
SECCOMP_RET_ALLOW = 0x7FFF0000
SECCOMP_RET_ERRNO = 0x00050000
EPERM = 1

# The io_uring system calls have the same numbers on every architecture.
IO_URING_SYSCALLS = (425, 426, 427)


def instruction(code, jt, jf, k):
    return struct.pack("HBBI", code, jt, jf, k)


def filter_program():
    program = [instruction(BPF_LD_W_ABS, 0, 0, 0)]  # seccomp_data.nr
    count = len(IO_URING_SYSCALLS)
    for i, nr in enumerate(IO_URING_SYSCALLS):
        # Equal: jump to the "deny" instruction after the remaining checks
        # and the "allow".
        program.append(instruction(BPF_JEQ_K, count - i, 0, nr))
    program.append(instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW))
    program.append(instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | EPERM))
    return b"".join(program), len(program)


class SockFprog(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]


def main():
    if len(sys.argv) < 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    libc = ctypes.CDLL(None, use_errno=True)
    code, count = filter_program()
    buf = ctypes.create_string_buffer(code, len(code))
    prog = SockFprog(count, ctypes.cast(buf, ctypes.c_void_p))
    if libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_NO_NEW_PRIVS)")
    if libc.prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ctypes.byref(prog), 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_SECCOMP)")
    os.execvp(sys.argv[1], sys.argv[1:])


if __name__ == "__main__":
    sys.exit(main())
