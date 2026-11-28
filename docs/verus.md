# Verus proofs

[Kani](kani.md) proves the unsafe core for every input within stated bounds (5 slots, 2
buffers), and loom checks the wake-up handshakes for two or three threads. [Verus](https://github.com/verus-lang/verus) proves properties for *every* input
with no bound at all: it turns the code and its specification into formulas that an SMT
solver (Z3) checks once for all values. The proofs live in `verify/`, outside every crate:
no user of giong compiles them, downloads Verus or gets a new dependency, and the code
they prove is the code that ships, unchanged (0 instructions of difference).

```sh
python3 scripts/verus.py install   # the pinned Verus into target/verus, and Rust 1.100.0
python3 scripts/verus.py all       # drift, prove, planted (about 10 s)
scripts/check.sh verus             # the same
```

CI: the `lint` job runs `scripts/verus.py drift` on every pull request (Python only, under a
second); the `verus` job installs Verus and runs `all` in the merge queue, and on a pull
request that changes `verify/` or `scripts/verus.py`. Both proofs take 5 seconds, the
eight planted bugs about 20 more; the job's minutes are mostly the runner's setup.

## What is proven

| Proof | Property | Bounds | Time |
| --- | --- | --- | --- |
| `verify/wakeups.rs` | **A worker never forgets a task.** In every execution in which the worker and each waking thread get to run (weak fairness), every wake-up of a live task is followed by a poll of that task that sees it, unless the task finishes first. The model has `RunFlag` (`try_enqueue`, `begin_poll`), `Doorbell` (`ring`, `take`, `prepare_park`, `unpark`), the injector, the run queue and its batches, the eventfd and the worker's sleep, with spawns, wake-ups from the worker's own thread and spurious returns from the kernel allowed at any time. Under it, an invariant (no task is queued without being in a queue or on its way; no unseen wake-up is uncovered; injected work is announced; a sleeping worker is woken) holds in every reachable state. | None: any number of tasks, waking threads, queued entries and steps (unbounded executions, infinite time) | 3 s |
| `verify/ipc_ring.rs` | For every capacity from 4 KiB to 1 GiB, any sequence of calls and **any value the peer process writes** into the shared memory (both positions and every record head): `push`, `has_room`, `peek`, `release` and `has_data` never read or write outside the direction's data area; never panic (no arithmetic overflow or underflow, no division by zero, the `debug_assert!` in `push` holds); `peek`'s loop ends; the record `peek` returns lies inside the data area, before the producer's checked tail, and is aligned; each side's position only grows, stays 8-byte aligned and at most one ring apart from the other side's checked position; a producer never moves past the consumer's checked head plus the capacity. | None: all capacities, all sequence lengths (the invariant is inductive), all peer behaviour | 2 s |

The untrusted peer is modelled exactly as it is: vstd specifies the standard atomics with
no knowledge of what a load returns, so every `head` and `tail` the ring loads is an
arbitrary `u64`, and every record head it reads is an arbitrary `u32`. Nothing in the proof
assumes the peer follows the protocol.

**What it found.** A wrap marker (a record head of `u32::MAX`) at offset 0 was followed
like any other: the consumer skipped a whole ring and looked again. No producer writes one
there (every record fits from offset 0), but a peer that did, and kept moving `tail` one
ring ahead, could keep `peek` looping, and with it the worker, for as long as it kept
winning the race. Verus could not prove that the loop ends; `peek` now reports the marker
as corrupt (`GIONG-E0091`), and the loop runs at most twice. The test
`a_wrap_marker_at_the_start_of_the_data_area_is_reported` covers it.

**Planted bugs.** `scripts/verus.py planted` puts each of these into a copy of the proof and
fails unless Verus rejects it, so a proof that has become vacuous (proving anything) is
caught by CI:

- `run_batch` polling a task without `RunFlag::begin_poll` (the flag stays set, so every
  later wake-up skips the queue);
- `prepare_park` not looking at `pending` after announcing the sleep;
- `Doorbell::ring` taking the `sleeping` flag but writing no eventfd;
- the worker sleeping with tasks still in its run queue;
- `checked_head` accepting a head more than one ring behind the tail (the producer's room
  underflows);
- `peek` following a wrap marker at offset 0 (the loop does not end);
- `peek` returning a record that runs past the end of the data area;
- `push` forgetting the room a wrap marker skips (it would pass the consumer's head).

## A worker never forgets a task

`verify/wakeups.rs` is a model, not the code: one step per atomic operation of the code,
under sequential consistency. The C++ memory model's weaker outcomes for the same
handshakes are loom's job (`scripts/check.sh loom`, `src/notify.rs`), for two or three
threads; Verus covers what loom cannot, any number of tasks, threads and steps, forever.

The worker's steps follow `Worker::drive` and `run_batch`: `drain_injected` (`take`, then
the injector's contents onto the run queue), the batch (its budget is the queue's length;
each entry popped is skipped if its task is gone, else `begin_poll` and a poll that reads
the task's state and may finish it), then either another turn (tasks are runnable, or the
spin loop, a probe or a timer found work) or `prepare_park` (swap `sleeping`, read
`pending`; a waiting balanced job also stops the sleep) and a sleep until the eventfd is
written, and `unpark`. Where the code's path depends on something the model leaves out
(a poll's result, what the spin loop sees, balanced jobs), the model lets either path
happen. A waking thread's steps follow `wake_by_ref` and `inject`: publish the task's
new state, `try_enqueue` (a swap), push into the injector, `ring` (swap `pending`, load
`sleeping`, swap it to elect one writer), write the eventfd.

The property is liveness, so it needs an assumption about scheduling: **weak fairness**,
the worker and each waking thread, when able to take a step, eventually take it. Nothing
else: other threads may start wake-ups or not, the worker's own wake-ups and spawns and the
kernel's spurious returns may come at any time and as often as they like. The proof
follows the classic WF1 rule from a wake-up to the poll: a waker that published calls
`try_enqueue`; a task that won it is pushed into the injector; the waker that pushed rings
the doorbell; a worker asleep after the ring is woken because one waker saw `sleeping` and
writes the eventfd; the worker comes round to `drain_injected`; and a task in the run queue
gets nearer the front with each pop (entries are only ever added behind it) until its poll
reads the new state. `assumptions_are_satisfiable` builds an execution that meets every
assumption with a task waiting, so the theorem is not vacuously true.

The code it models is pinned: `verify/pins.txt` holds a hash of the tokens of each
function the model describes (`RunFlag`, `Doorbell`, `wake_by_ref`, `task_waker`,
`run_batch`, `inject`, `drive`, `drain_injected`). A change to any of them fails the `lint`
job's drift check until someone has checked the model against it and updated the line
(`scripts/verus.py pins` prints the new ones). That is a prompt for review, not a proof
that model and code agree.

Proof size: the model is 202 lines for about 223 lines of pinned code (much of `drive` and
`run_batch` is timers, profiling and I/O the model leaves out); the invariant, the fairness
rules, the liveness chain and the witness are 1,370 lines: about 6 lines of proof per line
of code, between Atmosphere (about 3:1) and Lion's liveness proof of a runtime (about
12:1).

## Proof size of the ring

213 lines of code in `src/ipc/ring.rs` (without its tests), 248 lines of specification,
proof and trusted model in `verify/ipc_ring.rs`: 1.2 lines of proof per line of code. A
ring of integer positions is at the easy end.

## How the code and the proof stay one

`verify/ipc_ring.rs` holds, between `// verus-mirror: begin` and `// verus-mirror: end`, the
whole of `src/ipc/ring.rs` except its tests, with Verus annotations added: `requires` and
`ensures` on functions, `invariant` and `decreases` on loops, `proof { }` blocks, named
return values (`-> (r: T)`), and `#[verifier::...]` attributes. `scripts/verus.py drift`
removes exactly those, drops comments and the tests module, and compares the two token by
token. Visibility is not compared (Verus wants `pub` on a const the source declares
`pub(crate)`). Any other difference fails the `lint` job with the first tokens that differ.

So a change to `src/ipc/ring.rs` is made twice: in the source, and the same edit in the
mirror. Then `scripts/verus.py all` says whether the proof still holds; if it does not,
either the change broke a property (fix the change) or the proof needs another step (add
it in a `proof { }` block, or a lemma above the mirror).

Rules the drift check enforces:

- The executable code inside the mirror is the source's: proof code goes in `proof { }`
  blocks (an `assert` outside one is refused), on lines of its own, and spec clauses each
  start with the keyword alone on its line, ending at the line that opens the body (the
  layout `verusfmt` produces).
- Nothing may be trusted without a reason: an `#[verifier::external_body]` or
  `#[verifier::external]` item must be listed in `TRUSTED` in `scripts/verus.py`, with why,
  and `assume(` and `admit(` are refused anywhere.

## What is trusted

What Verus takes on faith, in `verify/ipc_ring.rs`'s `mem` module and in `TRUSTED`:

- **Raw memory.** `writable(p, n)` says `n` bytes from `p` lie in one allocation this
  process may use. Two axioms: a block's front part, and the `m` bytes `k` into it, are
  writable too. `pointer::add` requires the block it moves within; `copy_nonoverlapping`
  requires a readable source and a writable destination; `<[T]>::as_ptr` and
  `as_mut_ptr` give their slice's length. The source and destination of each copy never
  overlap (a local array or the caller's message, and the mapping); that is not proven.
- **`Ring::new`**: the constructor casts the mapping's base and takes the address of a
  `Dir`; its `ensures` restates its `# Safety` contract (a mapping of the right length, a
  capacity in range).
- **`Ring::dir`**: dereferences the shared `Dir`. Only its atomics are read through it.
- **The wake-up model** (`verify/wakeups.rs`) is trusted to describe the pinned code; see
  above.
- **Verus, Z3 and rustc** themselves.

## What is not covered

- **Weak memory orderings** in the wake-up handshakes: loom checks those, for small
  bounds. Verus assumes each atomic operation takes effect at once, in one order.
- **What the wake-up model leaves out**: that a balanced job itself is taken (`JobCount`,
  `spawn_balanced`; only its effect on the sleep is modelled), `Inject::Run`, `Shutdown`
  and `Poke`, a closed injector, the reuse of slots and of waker states (a stale entry that
  polls the slot's next task is an extra poll, harmless), and polls that never return (a
  task that blocks its worker is outside any runtime's promise). Completions the driver
  handles on the way out of a sleep wake tasks before `unpark`; the model has them just
  after it, which is the same (they touch neither `pending` nor `sleeping`).
- **The callers in `src/ipc.rs`.** That `release` gets the length `peek` returned, that a
  message is at most `max_message` long (checked there before `push`), and the sleeping
  handshakes, are not proven. Each is a precondition the proof states (`release`'s
  `requires`, `push`'s `requires`).
- **The producer's writes are not tracked one by one.** The proof shows the producer's
  position never passes the consumer's checked head plus the capacity, and that each write
  lies inside the data area; that each write lies between the old and the new position is
  plain from the code, not proven.
- **Concurrency and memory ordering.** Every load is arbitrary, so no interleaving of the
  peer's writes can break what is proven. Whether an honest peer's records arrive intact
  (ordering of the data writes against the `tail` store) is the job of the tests and of
  TSan.
- **Fewer than 2^64 - 2^30 bytes sent.** The producer's position is a `u64`; `push`
  requires that it has room to grow (centuries at memory speed). The consumer's position is
  bounded by the tail it checked, so no peer can make it overflow.

## Verus version

`VERUS_VERSION` in `scripts/verus.py` pins the release (0.2026.11.22.1ce6a59), and
`VERUS_SHA256` the sha256 of its archive: `install` refuses an archive with another hash,
or one with a file outside `verus-x86-linux/`, so a replaced release asset never runs in
CI or on a developer's machine. Each Verus release is built against one Rust toolchain
(`VERUS_TOOLCHAIN`, 1.100.0, CI's `RUST_STABLE`). To move to a newer Verus: change all
three (the new archive's sha256 from a download you have checked), run `scripts/verus.py install` and
`all`, and fix what the new version rejects (its syntax still moves: this release wants
`final(self)` for the value of `&mut self` in an `ensures`).
