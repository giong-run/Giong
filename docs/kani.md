# Kani proofs

giong's `unsafe` core is small: the slab that tracks operations, the receive buffer pool
the kernel writes into, `KernelBox`, and the rules that keep an operation's memory alive
until the kernel is done with it. [Kani](https://github.com/model-checking/kani), a model
checker for Rust, proves properties of that code for every input within stated bounds,
where a test or Miri checks one run. CI's `kani` job runs every proof on every change:

```sh
cargo install --locked kani-verifier --version 0.68.0 && cargo kani setup
scripts/check.sh kani        # all proofs; KANI_JOBS=2 runs two at a time (16 GB)
cargo kani --lib -p giong --harness packed_ring_buffers_are_never_shared_or_lost
```

The proofs live next to the code, in `#[cfg(kani)] mod proofs` (Kani sets `cfg(kani)`;
nothing in them is compiled otherwise). Kani's memory model also checks every pointer
access on the way: a read or write out of bounds, through a dangling pointer or after a
free, and a double free, fail a proof.

## What is proven

| Proof | Property | Bounds | Time |
| --- | --- | --- | --- |
| `slab::proofs` (5) | From any well-formed slab, `insert` takes a free slot and changes no other (proven in its two parts: `make_room`, when the free list's head is held back or the list is empty, leaves a vacant head below `end` and grows the table only when no slot is free, held-back ones included; `fill` then takes that slot), `remove` frees exactly its slot, `trim` keeps every value, cuts the vacant tail and hands out the lowest free key next, `take_all` returns every value; the free list never points at an occupied slot (the `unreachable!` in `insert`), every vacant slot below `end` is on it, and the slab stays well formed. | Up to 5 slots; any sequence length (inductive) | 4 min |
| `kbox::proofs` (2) | A `KernelBox`'s pointer stays valid while the box moves into an operation and on to the driver as `Box<dyn Any>`; the kernel's write through it is read back; the drop frees once. A path's C string is readable through `c_ptr` after the box moves. | One value | 1 s |
| `driver::uring::proofs` | For one single-shot operation, plain or zero-copy send, any results: the slab entry stays until the kernel's last completion (a reused key would hand a completion to another operation) and goes once both sides are done; resources of a dropped future, and a zero-copy send's hold, are released only after the last completion (the notification), exactly once; the future gets the result the kernel posted, once; a descriptor an open produced after its future was dropped is closed exactly once. | Every order of the events (see below) | 3.5–5 min, 6.6 GB |
| `buf::proofs` (3) | From any valid pool state, after any event (the kernel takes a buffer from either ring, user space reads or releases a piece, the driver arms or refills, the epoll driver takes a spare buffer): every buffer is in exactly one place (a ring, the spare list, or user space; only the packed ring's head buffer can be in its ring and held, and the kernel writes it only past the pieces handed out), so the kernel never gets a buffer user space reads, nor the same buffer twice; no buffer is lost; the pool's counts agree with the rings; a ring never holds more entries than slots; every ring entry names its buffer's real address and length. | 2 buffers of 2 bytes, epoll / whole ring / both rings; any sequence length (inductive) | 3–5.5 min, up to 5 GB each |

Each proof was checked against a planted bug, which it caught: a `remove` that does not
link the freed slot (`slab.well_formed()` fails), a release that recycles a buffer still at
the head of the packed ring ("a buffer is in the rings twice"), a zero-copy send whose
leftovers are released at its result instead of its notification ("released before the
notification"), and a write through a `KernelBox` pointer after its drop ("deallocated
dynamic object").

**Inductive proofs.** The slab and buffer-pool proofs start from *any* state that their
invariant accepts (`Slab::well_formed`, `BufPool::check`), apply one operation and check
the invariant again. The empty slab and the new pool satisfy it, so the proofs cover every
sequence of operations, of any length, that stays within the bounds.

**The operation's events.** The lifecycle proof lets three events happen in any order
(the kernel posts a completion; the future polls; the future is dropped), then the kernel
posts what is left and the future polls once more. Three are enough for every order: at
most three events change the state (two completions, and either the poll that collects
the result or the drop), and a poll that finds the operation pending changes nothing but
the waker.

## What is not covered

- **Aliasing rules.** Kani does not model Stacked or Tree Borrows; the `miri` job covers
  them for the same code.
- **Larger bounds.** The buffer pool is proven for 2 buffers, the slab for 5 slots. The
  code has no branch on the number of buffers beyond the ring index mask and the targets,
  but that is an argument, not a proof.
- **The kernel itself.** The buffer-ring proofs use a model of what io_uring does with a
  ring (it takes entries from the head in order; the packed ring keeps a buffer while room
  is left), and each kernel step is atomic between driver steps. The operation proof's
  kernel posts one completion, or a result then a notification for a zero-copy send.
- **The code around the proven functions.** The `Op` future's own code (the thread-local
  worker lookup, the retry on `EAGAIN`, the copy fallback on `EOPNOTSUPP`), the SQEs
  submitted (`AsyncCancel`, `Close`), which completions reach the pool, the backpressure
  policy, multishot operations, and the epoll driver's operation lifecycle.
- **Concurrency.** Everything here runs on one worker thread; the cross-thread wake-ups are
  model-checked with loom (`scripts/check.sh loom`).

## How the code is shaped for Kani

Kani gives up (runs out of memory) on some constructs, and the proven code is arranged so
that it can follow the real functions rather than copies of them:

- The single-shot lifecycle functions (`poll_single`, `cancel_single`, `Uring::single_cqe`,
  `Uring::complete`) take the slab and the graveyard as arguments, and are generic over the
  slab entry (`SingleSlot`) and what a dropped operation leaves behind (`Leftover`). The
  driver uses them with `OpState` and `Box<dyn Any>`, as before; the proofs use bare
  `Lifecycle`s and a concrete type. Following the drop code of a `Box<dyn Any>` means the
  drop code of every type the crate ever boxes that way, recursively through
  `ZeroCopyHold`, and an `OpState`'s drop includes a multishot queue's: both exhaust Kani.
  The instruction counts of `scripts/instr.py` did not change.
- Under `cfg(kani)` the pool's memory comes from the heap instead of `mmap`, and
  `page_size` is a constant: Kani cannot call foreign functions.
- `BufPool::new` builds the pool with `with_memory` after its size checks; the proofs call
  `with_memory`, since the coded errors of the checks drop `dyn Error`s.
- The proofs keep the pool in a local variable, not an `Rc`: Kani tracks a local's fields
  precisely but merges a heap object's fields across paths, after which every pointer read
  from it may point anywhere. They read ring entries at constant offsets and pick by index,
  and the slab proofs reserve the slab's capacity, for the same kind of reason.
