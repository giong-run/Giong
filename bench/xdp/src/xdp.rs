//! A minimal AF_XDP echo server with raw system calls (no libbpf/libxdp):
//! one UMEM, one socket bound to queue 0, an XDP program that redirects
//! every frame into it, and a loop that turns each UDP frame around in place.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

const SOL_XDP: i32 = 283;
const XDP_MMAP_OFFSETS: i32 = 1;
const XDP_RX_RING: i32 = 2;
const XDP_TX_RING: i32 = 3;
const XDP_UMEM_REG: i32 = 4;
const XDP_UMEM_FILL_RING: i32 = 5;
const XDP_UMEM_COMPLETION_RING: i32 = 6;
const XDP_PGOFF_RX_RING: i64 = 0;
const XDP_PGOFF_TX_RING: i64 = 0x8000_0000;
const XDP_UMEM_PGOFF_FILL_RING: i64 = 0x1_0000_0000;
const XDP_UMEM_PGOFF_COMPLETION_RING: i64 = 0x1_8000_0000;
const XDP_COPY: u16 = 1 << 1;
const XDP_ZEROCOPY: u16 = 1 << 2;

const FRAMES: u32 = 4096;
const FRAME: u32 = 2048;
const RING: u32 = 2048;

#[repr(C)]
#[derive(Default, Debug)]
struct RingOff {
    producer: u64,
    consumer: u64,
    desc: u64,
    flags: u64,
}
#[repr(C)]
#[derive(Default, Debug)]
struct MmapOffsets {
    rx: RingOff,
    tx: RingOff,
    fr: RingOff,
    cr: RingOff,
}
#[repr(C)]
struct UmemReg {
    addr: u64,
    len: u64,
    chunk_size: u32,
    headroom: u32,
    flags: u32,
    tx_metadata_len: u32,
}
#[repr(C)]
struct SockaddrXdp {
    family: u16,
    flags: u16,
    ifindex: u32,
    queue_id: u32,
    shared_umem_fd: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    options: u32,
}

/// A single-producer/single-consumer ring shared with the kernel.
struct Ring<T> {
    producer: *const AtomicU32,
    consumer: *const AtomicU32,
    desc: *mut T,
    mask: u32,
}

impl<T: Copy> Ring<T> {
    unsafe fn map(fd: i32, off: &RingOff, pgoff: i64) -> Self {
        let len = off.desc as usize + RING as usize * std::mem::size_of::<T>();
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            pgoff,
        );
        assert!(
            p != libc::MAP_FAILED,
            "mmap ring: {}",
            io::Error::last_os_error()
        );
        let b = p as *mut u8;
        Ring {
            producer: b.add(off.producer as usize) as *const AtomicU32,
            consumer: b.add(off.consumer as usize) as *const AtomicU32,
            desc: b.add(off.desc as usize) as *mut T,
            mask: RING - 1,
        }
    }
    fn prod(&self) -> &AtomicU32 {
        // SAFETY: points into the live mapping.
        unsafe { &*self.producer }
    }
    fn cons(&self) -> &AtomicU32 {
        // SAFETY: as above.
        unsafe { &*self.consumer }
    }
    fn slot(&self, i: u32) -> *mut T {
        // SAFETY: masked index inside the ring.
        unsafe { self.desc.add((i & self.mask) as usize) }
    }
}

fn check(rc: i64, what: &str) -> i64 {
    assert!(rc >= 0, "{what}: {}", io::Error::last_os_error());
    rc
}

/// bpf(2) with a byte buffer as `union bpf_attr`.
fn bpf(cmd: i64, attr: &mut [u8]) -> i64 {
    // SAFETY: the kernel reads at most attr.len() bytes.
    unsafe { libc::syscall(libc::SYS_bpf, cmd, attr.as_mut_ptr(), attr.len()) }
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

/// XSKMAP with one entry pointing at `xsk`, and an XDP program
/// `return bpf_redirect_map(&map, ctx->rx_queue_index, XDP_PASS)`, attached
/// to `ifindex` in driver mode (veth runs XDP natively).
fn attach_program(ifindex: u32, xsk: i32) -> i32 {
    // BPF_MAP_CREATE: map_type BPF_MAP_TYPE_XSKMAP (17), key 4, value 4, 64 entries.
    let mut a = [0u8; 128];
    put32(&mut a, 0, 17);
    put32(&mut a, 4, 4);
    put32(&mut a, 8, 4);
    put32(&mut a, 12, 64);
    let map = check(bpf(0, &mut a), "BPF_MAP_CREATE xskmap") as i32;
    // BPF_MAP_UPDATE_ELEM: map[0] = xsk.
    let key: u32 = 0;
    let val: u32 = xsk as u32;
    let mut a = [0u8; 128];
    put32(&mut a, 0, map as u32);
    put64(&mut a, 8, &key as *const u32 as u64);
    put64(&mut a, 16, &val as *const u32 as u64);
    check(bpf(2, &mut a), "BPF_MAP_UPDATE_ELEM");

    // Instructions: code, regs (dst | src << 4), off, imm.
    fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; 8] {
        let mut i = [0u8; 8];
        i[0] = code;
        i[1] = dst | (src << 4);
        i[2..4].copy_from_slice(&off.to_ne_bytes());
        i[4..8].copy_from_slice(&imm.to_ne_bytes());
        i
    }
    let prog: Vec<[u8; 8]> = vec![
        insn(0x61, 2, 1, 16, 0),  // r2 = *(u32 *)(r1 + 16): rx_queue_index
        insn(0x18, 1, 1, 0, map), // r1 = map (BPF_PSEUDO_MAP_FD)
        insn(0, 0, 0, 0, 0),      //   (second half of the 64-bit load)
        insn(0xb7, 3, 0, 0, 2),   // r3 = XDP_PASS (action when no socket)
        insn(0x85, 0, 0, 0, 51),  // call bpf_redirect_map
        insn(0x95, 0, 0, 0, 0),   // exit
    ];
    let code: Vec<u8> = prog.concat();
    let license = b"GPL\0";
    let mut log = vec![0u8; 4096];
    // BPF_PROG_LOAD: prog_type XDP (6), insn_cnt, insns, license, log_level,
    // log_size, log_buf, kern_version, prog_flags, prog_name[16],
    // prog_ifindex, expected_attach_type BPF_XDP (37).
    let mut a = [0u8; 128];
    put32(&mut a, 0, 6);
    put32(&mut a, 4, prog.len() as u32);
    put64(&mut a, 8, code.as_ptr() as u64);
    put64(&mut a, 16, license.as_ptr() as u64);
    put32(&mut a, 24, 1);
    put32(&mut a, 28, log.len() as u32);
    put64(&mut a, 32, log.as_mut_ptr() as u64);
    a[48..56].copy_from_slice(b"xsk_echo");
    put32(&mut a, 68, 37);
    let fd = bpf(5, &mut a);
    if fd < 0 {
        let e = io::Error::last_os_error();
        let end = log.iter().position(|&b| b == 0).unwrap_or(0);
        panic!(
            "BPF_PROG_LOAD: {e}\n{}",
            String::from_utf8_lossy(&log[..end])
        );
    }
    // BPF_LINK_CREATE: prog_fd, target_ifindex, attach_type BPF_XDP, flags
    // XDP_FLAGS_DRV_MODE (4). The link detaches when the process exits.
    let mut a = [0u8; 128];
    put32(&mut a, 0, fd as u32);
    put32(&mut a, 4, ifindex);
    put32(&mut a, 8, 37);
    let skb = std::env::var("XDP_ATTACH").is_ok_and(|v| v == "skb");
    put32(&mut a, 12, if skb { 2 } else { 4 });
    check(bpf(28, &mut a), "BPF_LINK_CREATE xdp") as i32
}

pub fn server(iface: &str) {
    let name = std::ffi::CString::new(iface).unwrap();
    // SAFETY: valid C string.
    let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
    assert!(ifindex != 0, "no interface {iface}");
    // SAFETY: the rest is the AF_XDP setup sequence; every pointer handed to
    // the kernel points at live memory of the size given.
    unsafe {
        let fd = check(libc::socket(44, libc::SOCK_RAW, 0) as i64, "socket(AF_XDP)") as i32;
        let umem_len = (FRAMES * FRAME) as usize;
        let umem = libc::mmap(
            std::ptr::null_mut(),
            umem_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            -1,
            0,
        ) as *mut u8;
        assert!(umem as *mut libc::c_void != libc::MAP_FAILED);
        let reg = UmemReg {
            addr: umem as u64,
            len: umem_len as u64,
            chunk_size: FRAME,
            headroom: 0,
            flags: 0,
            tx_metadata_len: 0,
        };
        let so = |opt: i32, v: *const libc::c_void, l: usize| {
            check(
                libc::setsockopt(fd, SOL_XDP, opt, v, l as u32) as i64,
                "setsockopt",
            )
        };
        so(
            XDP_UMEM_REG,
            &reg as *const _ as *const _,
            std::mem::size_of::<UmemReg>(),
        );
        let n: u32 = RING;
        for opt in [
            XDP_UMEM_FILL_RING,
            XDP_UMEM_COMPLETION_RING,
            XDP_RX_RING,
            XDP_TX_RING,
        ] {
            so(opt, &n as *const u32 as *const _, 4);
        }
        let mut off = MmapOffsets::default();
        let mut l = std::mem::size_of::<MmapOffsets>() as u32;
        check(
            libc::getsockopt(
                fd,
                SOL_XDP,
                XDP_MMAP_OFFSETS,
                &mut off as *mut _ as *mut _,
                &mut l,
            ) as i64,
            "offsets",
        );
        let rx: Ring<Desc> = Ring::map(fd, &off.rx, XDP_PGOFF_RX_RING);
        let tx: Ring<Desc> = Ring::map(fd, &off.tx, XDP_PGOFF_TX_RING);
        let fill: Ring<u64> = Ring::map(fd, &off.fr, XDP_UMEM_PGOFF_FILL_RING);
        let comp: Ring<u64> = Ring::map(fd, &off.cr, XDP_UMEM_PGOFF_COMPLETION_RING);

        // Half the frames go to the fill ring; the other half are free for
        // nothing here (echo reuses the received frame), kept as slack.
        let mut free: Vec<u64> = (0..FRAMES).map(|i| (i * FRAME) as u64).collect();
        let mut fp = fill.prod().load(Ordering::Relaxed);
        for _ in 0..RING {
            *fill.slot(fp) = free.pop().unwrap();
            fp = fp.wrapping_add(1);
        }
        fill.prod().store(fp, Ordering::Release);

        let mode = std::env::var("XDP_MODE").unwrap_or_else(|_| "copy".into());
        let flags = if mode == "zerocopy" {
            XDP_ZEROCOPY
        } else {
            XDP_COPY
        };
        let sa = SockaddrXdp {
            family: 44,
            flags,
            ifindex,
            queue_id: 0,
            shared_umem_fd: 0,
        };
        check(
            libc::bind(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<SockaddrXdp>() as u32,
            ) as i64,
            "bind(AF_XDP)",
        );
        let _link = attach_program(ifindex, fd);
        eprintln!("xdp echo on {iface} (ifindex {ifindex}, {mode} mode)");

        let mut rc = rx.cons().load(Ordering::Relaxed);
        let mut tp = tx.prod().load(Ordering::Relaxed);
        let mut cc = comp.cons().load(Ordering::Relaxed);
        let mut idle_spins = 0u32;
        loop {
            // Completed transmits give their frames back to the fill ring.
            let cp = comp.prod().load(Ordering::Acquire);
            if cp != cc {
                let mut fp = fill.prod().load(Ordering::Relaxed);
                while cc != cp {
                    *fill.slot(fp) = *comp.slot(cc);
                    fp = fp.wrapping_add(1);
                    cc = cc.wrapping_add(1);
                }
                comp.cons().store(cc, Ordering::Release);
                fill.prod().store(fp, Ordering::Release);
            }
            let rp = rx.prod().load(Ordering::Acquire);
            if rp == rc {
                idle_spins += 1;
                if idle_spins > 64 {
                    // Nothing for a while: sleep until frames arrive.
                    let mut p = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    libc::poll(&mut p, 1, 100);
                    idle_spins = 0;
                }
                continue;
            }
            idle_spins = 0;
            let mut sent = 0;
            while rc != rp {
                let d = *rx.slot(rc);
                rc = rc.wrapping_add(1);
                let frame =
                    std::slice::from_raw_parts_mut(umem.add(d.addr as usize), d.len as usize);
                if turn_around(frame) {
                    *tx.slot(tp) = d;
                    tp = tp.wrapping_add(1);
                    sent += 1;
                } else {
                    // Not ours: hand the frame straight back to the fill ring.
                    let fp = fill.prod().load(Ordering::Relaxed);
                    *fill.slot(fp) = d.addr;
                    fill.prod().store(fp.wrapping_add(1), Ordering::Release);
                }
            }
            rx.cons().store(rc, Ordering::Release);
            if sent > 0 {
                tx.prod().store(tp, Ordering::Release);
                // Copy mode transmits only on a kick.
                libc::sendto(
                    fd,
                    std::ptr::null(),
                    0,
                    libc::MSG_DONTWAIT,
                    std::ptr::null(),
                    0,
                );
            }
        }
    }
}

/// Swaps Ethernet, IPv4 and UDP source and destination of a UDP frame to
/// our port. The IPv4 checksum stays valid: a swap does not change a ones'
/// complement sum.
fn turn_around(f: &mut [u8]) -> bool {
    if f.len() < 42 || f[12] != 0x08 || f[13] != 0x00 || f[23] != 17 {
        return false;
    }
    let ihl = ((f[14] & 0x0f) as usize) * 4;
    let u = 14 + ihl;
    if f.len() < u + 8 || u16::from_be_bytes([f[u + 2], f[u + 3]]) != super::PORT {
        return false;
    }
    for i in 0..6 {
        f.swap(i, 6 + i);
    }
    for i in 0..4 {
        f.swap(26 + i, 30 + i);
    }
    f.swap(u, u + 2);
    f.swap(u + 1, u + 3);
    // veth hands frames over with checksum offload (only the pseudo-header
    // sum filled in), and a frame sent back from user space has no offload
    // flag: send it without a UDP checksum, which IPv4 allows.
    f[u + 6] = 0;
    f[u + 7] = 0;
    true
}
