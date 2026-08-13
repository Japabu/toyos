# Capability endowment

## 1. Invariants

1. **A process holds exactly what its parent moved into it, and there is
   nothing it can name to get more.** Endowment at spawn and `SYS_HANDLE_SEND`
   over a connection already held are the only ways a handle enters a
   process's table. There is no service registry, no connect-by-name, and no
   pid that is authority.
2. **A handle a process does not hold is a bug in that process.** `BadHandle`,
   `Stale` and `WrongType` end the caller — exit 139 — rather than answering a
   word it can ignore. The one exception is the connector argument to
   `SYS_NAMESPACE_BUILD`: a `provides` name is a connector another process
   made, so a wrong type there answers a refusal instead of ending whoever
   received it.
3. **Rights only shrink.** A handle moves or duplicates with the rights it
   already carries or fewer, never more. A parent that wants to hand over less
   duplicates narrower first and endows the dup.
4. **A port's `Acceptor` and `Connector` exist before either the client or the
   server that will use them runs.** There is no instant at which a name is
   unbound, so there is nothing to retry and no timeout anywhere in this
   design.
5. **A namespace is immutable once built.** A handle to a namespace is a
   handle to a fixed set of names; narrowing one is building a new namespace,
   never mutating the one held.
6. **`handle_count` is not the `Arc` count.** Userland-visible lifecycle
   events — EOF, `Gone`, device reclaim — ride `handle_count`, drained by
   process teardown on the killer's CPU, so a thread killed mid-block cannot
   strand a peer's view of the object it held.
7. **A refusal returns what it refuses.** A batch of handles
   `SYS_HANDLE_SEND` cannot deliver — a dead peer, a full queue — stays in the
   sender's table; nothing here drops a capability as a side effect of
   reporting that it could not be delivered.

## 2. Objects and handles

Every kernel object is a plain `Arc<T>`; there is no custom refcounting, no
`Weak` in the object graph, and no `dyn` dispatcher hierarchy. Object-layer
dispatch is `KObjectRef`, a closed enum with no `_` arm: pipe ends, `Acceptor`,
`Connector`, `Namespace`, shared memory, files, io_uring rings, device claims,
processes, threads, `SysCap`, the console. Every object embeds
`ObjectCore { koid, handle_count, retired }`. `Koid` is a `NonZeroU64`
identity for diagnostics and kernel-internal keys — never an authority, and
never sent to another process as one.

`RawHandle(u32)` packs a 12-bit slot and a 20-bit generation; a slot at
generation 0 encodes as its bare index, so stdio is `0`, `1`, `2`. A slot that
reaches its maximum generation is retired rather than reissued, so the largest
handle value any table hands out is `0xFFFF_EFFF` — which is why
`SYS_PORT_CREATE` can pack two handles into one `u64`
(`(acceptor << 32) | connector`) with no risk of colliding with the top 256
values `SyscallError` reserves for itself.

`HandleTable::get::<T>(handle, rights) -> Result<Arc<T>, HandleError>` returns
an owned `Arc`; nothing borrows into the table across a lock release.
`HandleEntry` is `!Clone` and moves by value between containers.

Rights are a bitset — `DUP`, `TRANSFER`, `READ`, `WRITE`, `MAP`, `WAIT`,
`MANAGE`, `RT`, `DEVICE`, `LOG` — and only ever shrink as a handle travels. A
`DeviceClaim` is minted without `DUP`, which is what keeps at most one handle
to a claim alive at a time and makes endowing a claim a move rather than a
copy.

`on_zero_handles` runs exactly once, off a deferred per-CPU queue drained at
syscall exit, `do_schedule` entry and the idle loop — never under a lock.
`kill_process` and `exit` share `teardown_resources`, which drains the handle
table on the *killer's* CPU, so the same hook that fires on a clean exit fires
on a kill. `Acceptor`, `Connector`, `Namespace` and `ConnectionEnd` all release
this way, never through a guard living on a blocked thread's own stack — the
shape that leaks when another CPU kills the thread that owns it. The one
object that still has that shape: a thread blocked in `SYS_ACCEPT` is
registered on the port's wait queue from its own stack, the same as every
other blocking syscall's wait-queue registration in this kernel, and it leaks
identically when killed. Nothing here is particular to ports, and nothing here
closes it.

## 3. Ports, namespaces, and SysCap

A **port** is a pair: an `Acceptor`, which a server accepts connections from,
and a `Connector`, through which a client opens one. Two types rather than one
object with a direction right, so "accept from a service you were only given
access to" is a state that cannot be written rather than a runtime refusal.
Both ends share a queue and a wait list; the acceptor's last handle marks the
queue closed and drops it, and every queued client's pipe ends drop with it —
the next write any of them makes answers `Gone`, the next read `0`. A
connector whose port is already closed refuses at once rather than queuing for
a server that will never read. A connector with no clients right now is not a
server that has stopped, so losing all its handles does nothing.

`SYS_PORT_CREATE` needs no right — a port with no clients is not authority —
and answers a packed `(acceptor, connector)` pair. `MAX_PENDING_CONNECTIONS`
(32) bounds one port's unaccepted queue and is what a client past it sees as
`ResourceExhausted`; a queued connection costs nothing until the first byte is
written on it, at which point it costs its ring page.

`SYS_ACCEPT` answers a single handle and nothing else — no pid. A server that
wants to name its client reads the client's own claim from the protocol's
first frame, which is already the client's own claim about itself, or uses
the connection's own `RawHandle` as a local identifier: a handle names one
object for the life of the process holding it and designates nothing in any
other process's table, which is the same property a machine-wide identity
would give without an ABI to carry it.

A **namespace** holds `name → Arc<Connector>`, sorted by name, immutable after
construction: there is no insert, remove or replace, only a new namespace
built from an existing one. `SYS_NAMESPACE_BUILD` takes a base namespace
(needs `READ`) plus connectors to add (each needs `TRANSFER`) and answers a
handle to the result; `SYS_NAMESPACE_OPEN(ns, name)` needs `READ` on the
namespace and answers a connection, or `NotFound` for a name the namespace
does not carry. `MAX_NAMESPACE_ENTRIES` (64) and `MAX_SERVICE_NAME` (64 bytes)
refuse a caller past them by name; neither truncates. A namespace holds
`Arc<Connector>` rather than handle entries, because a connector's
`on_zero_handles` is a no-op — there is nothing for the namespace to keep
alive by counting.

A `SysCap` carries no state of its own; its authority is entirely in the
rights on the handle. Four of its rights gate a syscall directly:

| Right | Gates | Held by |
|---|---|---|
| `DEVICE` | `SYS_DEVICE_CLAIM` | `/bin/init`, and whatever it endows a `DUP` of |
| `RT` | `SYS_RT_ENTER` | init, and a manifest-declared `RT`-only dup |
| `MANAGE` | `SYS_PROCESS_OPEN` | init only |
| `LOG` | `SYS_LOG_READ` | init only |

`DUP` on a `SysCap` is itself an authority — over the capability, not over the
machine — held by whatever a manifest names to pass it on to its own children.
The kernel mints exactly one full-rights `SysCap`, at boot, into `/bin/init`'s
table; every other process's device-claiming, RT-entering, process-opening or
log-reading authority traces back to a dup init chose to endow.

## 4. The manifest

`/etc/system.manifest` is rendered at build time from `system.toml` and read
once, at boot, by `/bin/init` — the kernel spawns exactly one program, and
every other process exists because init read this file and decided to create
it. No other process parses it or holds it; a program never reads its own
row, it only asks init what it was given.

Each `[programs.<name>]` entry declares:

| key | meaning |
|---|---|
| `serves` | init creates one machine-wide port per name and endows this program the acceptor |
| `provides` | this program creates its own port for each of these, once per instance, and hands the connector to the children it spawns itself; init creates and holds nothing for these |
| `receives` | names in this program's namespace, each a connector |
| `devices` | device classes init mints a claim for and endows |
| `syscap` | rights on the `SysCap` duplicate init endows this program — `rt`, `device`, `dup`, named individually rather than as one switch, because each is a machine-wide authority that exists nowhere else |

`serves` and `provides` mean different things and a name is never declared
both ways. A `serves` name is one port for the whole machine — every client's
connector for it points at the same shared queue. A `provides` name is per
instance: init does not know in advance how many terminals will run, so it
cannot create `surface`'s port, and a single machine-wide one would let any
holder of the connector reach whichever instance happened to hold the
acceptor.

Every port for every `serves` name in the whole manifest exists before any
`[boot] start` program runs — including for a program nothing in
`[boot] start` names. `filepicker`'s acceptor exists at boot, endowed to it
only when the compositor first launches it, so a client already holding the
`filepicker` connector can write before the picker has run a single
instruction. `[boot] start` names `[programs]` keys, not paths, and orders
nothing, because there is no ordering left to express once every port already
exists.

A device class is claimed by at most one program; two programs declaring the
same `devices` entry is refused when the image is built, never arbitrated
first-come at runtime.

**The test estate's authority is a manifest fact.** The guest binaries under `tests/toyos-rust-tests/src/bin/` are not
`[programs]` entries, so no manifest row can name what any single one of them
needs. `test-runner` is the one program whose `receives` is the union of what
its binaries need in a given boot config; every binary it spawns inherits
test-runner's own namespace by the default `SYS_SPAWN` path (§6) rather than
by any row of its own — a test binary's authority is exactly test-runner's. A
test that needs a server it controls builds one directly, inside itself: a
port, a namespace over the connector, a child spawned holding it, no manifest
row and no name anything else can see.

## 5. The ABI

`toyos-abi/src/syscall.rs` is the source of truth for numbers and argument
shapes; every syscall that takes a handle documents the right it needs and
answers `PermissionDenied` without it, and there is no default. In outline:

| call | does |
|---|---|
| `SYS_ENDOWMENTS` | read this process's own `(label, handle)` table back |
| `SYS_PORT_CREATE` | mint an `Acceptor`/`Connector` pair |
| `SYS_NAMESPACE_BUILD` / `SYS_NAMESPACE_OPEN` | build a namespace from a base plus connectors / resolve a name in one held |
| `SYS_HANDLE_SEND` / `SYS_HANDLE_RECV` | move a batch of handles over a connection |
| `SYS_SHM_CREATE` / `SYS_SHM_MAP` / `SYS_SHM_UNMAP` | a shared-memory region as a handle, not a token |
| `SYS_PROCESS_WAIT` / `SYS_PROCESS_KILL` / `SYS_PROCESS_OPEN` | act on a `Process` handle, or mint one from a pid with `MANAGE` on a `SysCap` |
| `SYS_DEVICE_CLAIM` | mint a device claim, gated by `DEVICE` on a `SysCap` |
| `SYS_RT_ENTER` | enter the real-time band, gated by `RT` on a `SysCap` |
| `SYS_LOG_READ` | copy kernel log records, gated by `LOG` on a `SysCap` |

Number 113 is reserved for a port-rearm call that does not exist yet (§7);
nothing is built at it. A retired number — `SYS_WAITPID`, `SYS_LISTEN`,
`SYS_CONNECT`, `SYS_OPEN_DEVICE`, `SYS_KILL`, the shared-memory token quartet,
among others — is never reused, and its gravestone comment in
`toyos-abi/src/syscall.rs` states why.

`SYS_CLOSE` needs nothing on the handle it drops — dropping is not an
operation on the object it names. `SYS_FSTAT` and `SYS_MARK_TTY` need
nothing — the first answers what a handle is without moving its content, the
second is a statement about an end made by whoever created the pipe, and
neither end of a pipe carries the other's right. `SYS_HANDLE_DUP` and
`SYS_HANDLE_DUP_AT` need `DUP`, and the requested set must already be a
subset of the source's; `SYS_HANDLE_DUP_AT`'s answer carries the slot's own
generation, not the number that went in — the one place this ABI's `dup2`
deliberately disagrees with POSIX's.

Some syscalls stay ambient, deliberately: `SYS_GETPID` and every pid in a log
or a scheduler key are pure names, never authority. `SYS_THREAD_JOIN` still
takes a bare `Tid` rather than a handle, because a `Tid` never crosses a
process boundary and there is nothing to smuggle through one.
`SYS_SYSINFO`, `SYS_SCHED_INFO` and `SYS_SHUTDOWN` are gated by nothing; a
`SysCap` right for `SYS_SHUTDOWN` would have to reach the test estate through
`test-runner`, and nothing has asked for it yet.

`SYS_HANDLE_SEND` requires `TRANSFER` on every handle in its batch and
carries each one's rights into the connection's other end unchanged, the same
as endowment does. There is no way to send a handle with fewer rights than
the sender holds while withholding `TRANSFER` itself, so a handle that
arrives by either move path can always be moved on again by whoever receives
it — a bound on delegation, not a revocation, and the only one either path
expresses.

`SyscallError::Gone` is a fact about the *object*: the acceptor is gone, or
the peer end is. `SyscallError::NotFound` on `SYS_NAMESPACE_OPEN` is a fact
about the *caller*: the name was never in the namespace it was given. The two
are never conflated — a namespace-absent name and a closed port are different
facts to a client deciding whether to give up or whether it has a bug, and
only the kernel can tell them apart, because the SDK sees one word either
way. `toyos::EndowError` carries the same split, as `NotEndowed` and
`ServerGone`.

Every bound below is named `MAX_*`, sits on the primitive, and refuses a
caller past it with `InvalidArgument` — never truncates.

| constant | value | bounds |
|---|---|---|
| `MAX_ENDOWMENTS` | 32 | handles a spawn may endow |
| `MAX_NAMESPACE_ENTRIES` | 64 | names in one namespace |
| `MAX_SERVICE_NAME` | 64 bytes | one name |
| `MAX_PROGRAM_NAME` | 32 bytes | one `[programs]` key |
| `MAX_LABELS_LEN` | 4096 bytes | the endowment label blob |
| `MAX_PENDING_CONNECTIONS` | 32 | one port's unaccepted queue |
| `MAX_TRANSFER_HANDLES` | 8 | one `SYS_HANDLE_SEND` batch |
| `MAX_QUEUED_BATCHES` | 16 | per connection direction |
| `MAX_LAUNCH_EXTRAS` | 5 | connectors a caller may transfer with one launch |
| `MAX_LAUNCH_SLOTS` | 3 | stdio handles a launch carries |

## 6. Spawn and endowment

At boot the kernel creates one full-rights `SysCap`, spawns `/bin/init` with
it and three `SerialConsole` slots, and spawns nothing else. init reads the
manifest, creates a port for every `serves` name it declares anywhere, and
for every program in `[boot] start` mints its device claims, its `SysCap` dup
if it declares `syscap` rights, and its namespace, then spawns it holding all
of them. init itself serves `launcher` — not through any `[programs]` row,
because init is in every image and is not a `[programs]` key.

`SYS_SPAWN`'s argument carries two separate vectors for handles, and they
differ in kind, not only in name. A `slot_map` entry *duplicates* a handle
into the child — the parent keeps its own copy, which is how stdio crosses
every spawn. An `endow` entry *moves* a handle out of the parent's table
entirely. This is what makes endowing a `DeviceClaim` work with no special
case: a claim carries no `DUP` right, so a move is the only form the ABI can
express for it, and the parent provably no longer holds it once the child
does. Rights on an endowed handle are exactly the parent's — there is no
rights argument on the move, because a second place to shrink rights would
contradict the one `SYS_HANDLE_DUP` already is.

A process's own view of what it was endowed is `SYS_ENDOWMENTS`, parsed once
into `toyos::endow::Endowments`. Labels are local names in one process's own
table and answer nothing across a process boundary: `svc` names the
namespace, `syscap` the `SysCap` dup, `serve:<name>` an acceptor, `dev:<class>`
a device claim. `endow::service(name)` is the only place a name is resolved
anywhere in userland, and it answers `NotEndowed` or `ServerGone` — never a
third answer, and never "not yet".

**The launcher.** `SYS_SPAWN` alone is not enough to give a grandchild
authority its parent lacks — a shell that starts doom cannot endow sound if
the shell never held it — so init also serves `launcher`: a caller sends a
`[programs]` key, argv, env, cwd and the stdio handles and connectors it wants
to carry along, and gets back a `Process` handle over `SYS_HANDLE_SEND`. init
looks the name up in the manifest it already holds, builds the endowments
that row declares, unions in whatever connectors the caller transferred, and
spawns. This adds no authority: a caller can only transfer a handle it
already holds, and a caller who transfers nothing gets exactly the manifest
row. init keeps no `Process` handle from a launch — the send is a move — so a
`launcher` holder cannot exhaust init's own table by launching in a loop;
init holds process handles only for what `[boot] start` named.

The routing rule is three clauses, and every `Command` in the SDK and in std
follows them in order:

1. **A caller that endows anything itself uses `SYS_SPAWN` directly.**
   Deciding a child's authority by hand and asking the launcher to overwrite
   it with a manifest row would be a contradiction — a terminal spawning its
   shell with a fresh `surface` connector is this case.
2. **Otherwise, a caller holding a `launcher` connector asks the launcher**,
   which answers a named refusal for a program that is not a `[programs]`
   key. The SDK never parses the manifest itself — init holds it and answers
   about it, so there is never a second reader of that file able to disagree
   with the first.
3. **A caller with no `launcher` connector uses `SYS_SPAWN` directly, and
   inherits.** Unless the caller already endowed something under the `svc`
   label itself, std's `Command` duplicates the caller's own namespace handle
   and endows the duplicate under `svc` — a duplicate rather than the handle
   itself, since an endowment is a move and a parent that gave its namespace
   away could not spawn a second child. A program init never launched gets
   what its parent chose to give it.

An acceptor is endowed by move, so a `serves` program can be launched through
init at most once per boot: once init hands over the acceptor for a name, it
holds nothing for that name to give a second launch, and a second launch
request for it is a named refusal rather than a spawn with a missing
endowment. Nothing in this design restarts a `serves` server and gives its
existing clients a path to the replacement (§7).

## 7. Queueing and failure semantics

| server state | `SYS_NAMESPACE_OPEN` | first write | first read |
|---|---|---|---|
| not spawned yet | succeeds; connection queues on the port | goes into the ring | blocks |
| spawned, not yet at `accept` | succeeds | goes into the ring | blocks |
| accepted, alive | succeeds | ring | data |
| exited without accepting | `Gone` | `Gone` | `0` |
| exited after accepting | `Gone` | `Gone` | `0` |
| never in this namespace | `NotFound` (`NotEndowed` in the SDK) | — | — |

No row is reached by waiting: there is no timer, no deadline and no retry
constant anywhere in this design. A client already blocked in `read` when its
peer dies is woken by the zero-handle hook on the killer's CPU and observes
`0` on its own next scheduler pass — bounded by a scheduler pass, never by a
duration.

A server never blocks on a client: every server write is a `try_send` whose
refusal drops the peer by name, and structurally a server holds an
`Acceptor`, which has no write path at all.

**Teardown**, on both exit and kill, since both drain the handle table
through the same path:

| last handle of this kind goes | the peer sees |
|---|---|
| `Acceptor` | queued clients: `Gone`/EOF; new opens: `Gone` |
| `ConnectionEnd` | `Gone` on write, `0` on read |
| `Connector` | nothing — the server keeps accepting from whoever else holds one |
| `Namespace` | nothing observable; a connector it held outlives it if another handle to that connector exists |
| `DeviceClaim` | the class is released for re-minting by init |
| `SharedMemObject`, last mapping too | pages free |

**A dead `serves` port stays dead.** A namespace is immutable, so a process
whose namespace was built before a server crashed holds a connector onto a
`PortShared` that is closed forever and can never reach a replacement of that
server; init can only put a fresh port in the namespaces of programs it
launches afterward. Nothing in this design supervises a daemon or restarts
one, and no client re-resolves a name after its first connection, so the gap
is reachable only once something does both. The closing mechanism is a
port-rearm call at the reserved number 113 (§5): it would mint a fresh
acceptor for an existing `PortShared` and clear its closed flag, reachable at
once by every namespace already pointing at that `PortShared`. It does not
exist because nothing today would call it.

Two connections each queued inside the other's outbox leak both until reboot.
`SYS_HANDLE_SEND` refuses to send a handle naming the connection it is sent
on, which closes the one-hop case; nothing stops a longer cycle through two
separate sends.

## 8. Gates

Every gate below either fails on the tree with its defect present or has a
negative arm that does.

- **`every_receives_names_a_provider`** (`src/build.rs`, `cargo test --lib`) —
  for every program in every shipped `system.toml`, every `receives` name is
  some program's `serves` or `provides` in that same config. A client cannot
  be given a name the system it is built into does not have.
- **`a_provides_name_is_never_also_a_serves_name`** — the two mean different
  things (one port machine-wide, one per instance); declaring both is refused
  rather than left to produce a dead connector nobody can trace.
- **`every_device_class_has_at_most_one_claimant`** — two programs declaring
  the same `devices` entry is a config init cannot satisfy, refused before
  the image exists rather than arbitrated by whichever process starts first.
- **`no_diag_program_claims_the_screen`** — the diagnostic image's programs
  declare no `devices` at all, which is what "nothing in this image can reach
  the framebuffer" means now, and it is checkable rather than merely true of
  which binaries happen to be present.
- **`every_started_program_is_declared`** — every `[boot] start` entry is a
  `[programs]` key; a typo is a build failure, not a panic in a booted
  kernel.
- **`every_declared_capability_is_one_the_abi_has`** — every `syscap` name in
  every config is one `toyos_manifest::syscap_rights` recognizes.
- **`every_shipped_boot_config_is_covered`** — the gates above run over every
  `system.toml` the tree ships, asserted equal to what
  `find . -name system.toml` finds, so a new config with no row in these
  gates is a red rather than a silent gap.
- **`connect_before_serve`** (guest binary) — a client endowed a connector
  before its server is even spawned writes and reads through it once the
  server accepts, with the frame already buffered at the server's first
  `accept`; the same setup with the server exited immediately answers the
  client `Gone`/`0` in less time than any plausible timeout, proving no timer
  is involved.
- **`endowment_denied`** (guest binary) — a child endowed a namespace holding
  one name resolves that name and gets `NotFound` for a name it was not
  given; a sibling endowed both resolves both, which is what proves the first
  arm failed because the name was truly absent and not because the service
  never came up. `SYS_DEVICE_CLAIM` and `SYS_RT_ENTER` on a `SysCap` the
  caller was not endowed answer `PermissionDenied`.
- **`no_name_resolves_through_a_registry_any_more`** (`src/sourcegate.rs`,
  `cargo test --lib`) — the retired registry's identifiers (`SYS_CONNECT`,
  `SYS_LISTEN`, `SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_SOCKET_CREATE`,
  `SharedToken`, `services::connect`) appear nowhere in code outside the
  retired-number gravestones and `specs/`; comments and string literals are
  stripped before the scan runs, so a gravestone naming its own retired
  symbol does not trip it.
- **`handle_basic`** (guest binary) — a closed slot is reissued at the next
  generation and names nothing until then; a duplicate can only narrow
  rights, never widen them; a handle without `DUP` cannot be duplicated at
  all.
- **`handle_transfer`** (guest binary) — a batch `SYS_HANDLE_SEND` cannot
  deliver (dead peer, full queue) leaves the batch in the sender's table
  rather than dropping it as a side effect of the refusal; a batch delivered
  and never received releases through the queue and the per-kind census
  records it.
- **`kill_while_blocked`** (guest binary) — a thread killed while blocked
  reading a pipe, an IPC connection, or waiting on an `Acceptor` still
  releases its handle through the table drain, so the peer observes the same
  `Gone`/EOF a clean close would have produced. This is the gate that exists
  only because `handle_count` is not the `Arc` count.
- **`device_claim_lifetime`** / **`process_lifecycle`** / **`handle_kill_policy`**
  (guest binaries) — a device claim moves rather than copies and its class
  re-mints after the holder dies; a process's exit code is published to its
  `ProcessObject` exactly once and is readable by every handle to it
  thereafter, with no zombie and no reap; an operation on a handle the caller
  does not hold ends the caller (`BadHandle`/`Stale`/`WrongType` → exit 139)
  except `SYS_NAMESPACE_BUILD`'s added-connector argument, which answers a
  refusal instead.
- **Per-variant `LIVE_*` census** — every churn test asserts object counts
  back to baseline per kind, not as one machine-wide total, so a leak in one
  kind cannot hide inside churn in another.
