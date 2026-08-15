# Capability endowment

## 1. Invariants

1. **A process holds exactly what its parent moved into it**, and there is
   nothing it can name to get more. Endowment at spawn and `SYS_HANDLE_SEND`
   over a connection already held are the only ways a handle enters a
   process's table. There is no service registry, no connect-by-name, and a
   pid is never authority.
2. **A handle a process does not hold is a bug in that process.**
   `BadHandle`, `Stale` and `WrongType` end the offending process (exit code
   139) rather than returning an error it could ignore. The one exception is
   the connector argument to `SYS_NAMESPACE_BUILD`: a `provides` name is a
   connector another process made, so a wrong type there answers
   `WrongType` as a refusal instead of ending the receiver.
3. **Rights only shrink.** A handle moves or duplicates with the rights it
   already carries or fewer, never more. A parent that wants to hand over
   less duplicates narrower first and endows the duplicate.
4. **A port's two ends exist before either the client or the server runs.**
   There is no instant at which a declared name is unbound, so no resolution
   ever waits, retries, or times out.
5. **A namespace is immutable once built.** A handle to a namespace is a
   handle to a fixed set of names; narrowing one is building a new namespace,
   never mutating the one held.
6. **A kill releases like an exit.** A peer of a killed process observes the
   same EOF and `Gone` a clean exit produces, within one scheduler pass of
   the kill — never after a timeout, because there is none.
7. **A refusal returns what it refuses.** A batch of handles
   `SYS_HANDLE_SEND` cannot deliver — a dead peer, a full queue — stays in
   the sender's table.

## 2. Handles and rights

A handle names one object in the holding process's table and designates
nothing in any other process's table. A closed slot is reissued at the next
generation, so a stale value names nothing; a slot at its maximum generation
is retired, never reissued. A slot at generation 0 encodes as its bare index
— stdio is `0`, `1`, `2` — and no handle value ever exceeds `0xFFFF_EFFF`,
which keeps the top 256 values of a result word free for errors.

Rights: `DUP` gates duplication; `TRANSFER` gates sending and endowing;
`READ` and `WRITE` gate an object's data operations; `MAP` gates mapping
shared memory; `WAIT` gates waiting on a process; `MANAGE`, `RT`, `DEVICE`
and `LOG` gate the §3 system-capability syscalls. A duplicate's requested
rights must be a subset of the source's.

A **device claim** is minted without `DUP`, so at most one handle to a claim
exists; endowing it is a move, and its class is claimable again only after
that handle's release (§7).

## 3. Ports, namespaces, and the system capability

A **port** is an `Acceptor`, from which a server accepts connections, and a
`Connector`, through which a client opens one. Both ends share one queue.
When the last `Acceptor` handle is released, the port closes forever: every
queued client's connection answers `Gone` on write and `0` on read, and a
later open through any connector answers `Gone` at once — no connection
queues for a server that cannot read it. Releasing connector handles has no
observable effect. `MAX_PENDING_CONNECTIONS` bounds one port's unaccepted
queue; a client past it observes `ResourceExhausted`.

`SYS_PORT_CREATE` requires no right and answers the pair packed in one
result. `SYS_ACCEPT` answers a single connection handle and nothing else —
no pid, no peer identity.

A **namespace** maps names to connectors, immutable after construction.
`SYS_NAMESPACE_BUILD` takes a base namespace (`READ`) plus connectors to add
(each `TRANSFER`) and answers the new namespace. `SYS_NAMESPACE_OPEN`
(`READ`) answers a connection, or `NotFound` for a name the namespace does
not carry. `NotFound` and `Gone` are never conflated: the first is a fact
about the caller's namespace, the second about the server's port.

A **system capability** carries no state; its authority is entirely in the
rights on the handle. `DEVICE` gates `SYS_DEVICE_CLAIM`, `RT` gates
`SYS_RT_ENTER`, `MANAGE` gates `SYS_PROCESS_OPEN`, `LOG` gates
`SYS_LOG_READ`. The kernel mints exactly one full-rights system capability,
at boot, into init's table; every other process's authority over the machine
traces back to a duplicate init endowed.

## 4. The manifest

The manifest is rendered at image build and read once, at boot, by init. The
kernel spawns only init; every other process exists because init created it.

Each program entry declares:

| key | meaning |
|---|---|
| `serves` | init creates one machine-wide port per name, before any program starts, and endows this program the acceptor |
| `provides` | this program creates its own port per name, once per instance, and hands the connector to children it spawns itself; init holds nothing for these |
| `receives` | the names in this program's namespace, each a connector |
| `devices` | device classes init mints a claim for and endows |
| `syscap` | the rights on the system-capability duplicate init endows, named individually |

Rules, all refused at image build:

- A name is never both `serves` and `provides`: the first is one port for
  the machine, the second one port per instance.
- Every `receives` name is some program's `serves` or `provides` in the same
  configuration.
- A device class has at most one claimant.
- Every boot-start entry is a declared program, and every declared `syscap`
  right is one the ABI defines.

Boot-start order expresses nothing: every `serves` port exists before any
program runs, so a client can hold — and write into — a connection whose
server has not yet started.

## 5. The ABI

A syscall that takes a handle requires a named right on it and answers
`PermissionDenied` without it. In outline:

| call | does |
|---|---|
| `SYS_ENDOWMENTS` | read the calling process's own label→handle table |
| `SYS_PORT_CREATE` | mint an `Acceptor`/`Connector` pair |
| `SYS_NAMESPACE_BUILD` / `SYS_NAMESPACE_OPEN` | build a namespace / resolve a name in one |
| `SYS_HANDLE_SEND` / `SYS_HANDLE_RECV` | move a batch of handles over a connection |
| `SYS_SHM_CREATE` / `SYS_SHM_MAP` | shared memory as a handle; a region's mappings go with its last handle, so there is nothing to unmap by hand |
| `SYS_PROCESS_WAIT` / `SYS_PROCESS_KILL` / `SYS_PROCESS_OPEN` | act on a process handle; mint one from a pid only with `MANAGE` |
| `SYS_DEVICE_CLAIM` | mint a device claim (`DEVICE`) |
| `SYS_RT_ENTER` | enter the real-time band (`RT`) |
| `SYS_LOG_READ` | copy kernel log records (`LOG`) |

A retired syscall number is never reused. Number 113 is reserved and
carries nothing.

`SYS_CLOSE`, `SYS_FSTAT` and `SYS_MARK_TTY` require no right.
`SYS_HANDLE_DUP` and `SYS_HANDLE_DUP_AT` require `DUP`;
`SYS_HANDLE_DUP_AT`'s answer carries the slot's current generation, not the
number that went in. `SYS_GETPID`, `SYS_THREAD_JOIN` (a thread id never
crosses a process boundary), `SYS_SYSINFO`, `SYS_SCHED_INFO` and
`SYS_SHUTDOWN` are ungated.

`SYS_HANDLE_SEND` requires `TRANSFER` on every handle in the batch and
delivers each with its rights unchanged. A handle that arrives by any move
path can always be moved on again: the rights system bounds delegation, not
onward transfer. A send may not include the connection it is sent on.

Bounds — each named constant refuses a caller past it with
`InvalidArgument`, except the pending-connection bound, which answers
`ResourceExhausted` (§3); none truncates:

| constant | value | bounds |
|---|---|---|
| `MAX_ENDOWMENTS` | 32 | handles one spawn may endow |
| `MAX_NAMESPACE_ENTRIES` | 64 | names in one namespace |
| `MAX_SERVICE_NAME` | 64 bytes | one name |
| `MAX_PROGRAM_NAME` | 32 bytes | one program key (enforced at image build) |
| `MAX_LABELS_LEN` | 4096 bytes | the endowment label blob |
| `MAX_PENDING_CONNECTIONS` | 32 | one port's unaccepted queue |
| `MAX_TRANSFER_HANDLES` | 8 | one `SYS_HANDLE_SEND` batch |
| `MAX_QUEUED_BATCHES` | 16 | per connection direction |
| `MAX_LAUNCH_EXTRAS` | 5 | connectors one launch may carry |
| `MAX_LAUNCH_SLOTS` | 3 | stdio handles one launch carries |

## 6. Spawn, endowment, and the launcher

`SYS_SPAWN` carries two handle vectors of different kind. A **slot** entry
*duplicates* a handle into the child — the parent keeps its own, which is
how stdio crosses every spawn. An **endow** entry *moves* a handle out of
the parent's table entirely; rights on an endowed handle are exactly the
parent's. A device claim, having no `DUP`, can only be endowed.

A process reads its own endowments back as labels: `svc` names its
namespace, `syscap` its system-capability duplicate, `serve:<name>` an
acceptor, `dev:<class>` a device claim. Resolving a service name answers the
connection, `NotEndowed`, or `ServerGone` — never a third state and never
"not yet".

init serves **`launcher`**: a caller sends a program key, arguments,
environment, working directory, and the stdio handles and connectors it
wants carried, and receives a process handle back over the connection. init
builds the endowments the program's manifest entry declares, unions in the
transferred connectors, and spawns. This adds no authority — a caller can
only transfer what it already holds — and init keeps no handle from a
launch. A key that is not a declared program is refused by name.

The routing rule for anything that spawns:

1. A caller that endows anything itself uses `SYS_SPAWN` directly.
2. Otherwise, a caller holding a `launcher` connector asks the launcher.
3. A caller with no `launcher` connector uses `SYS_SPAWN` and the child
   inherits: unless the caller endowed its own `svc`, the child receives a
   duplicate of the caller's namespace.

An acceptor is endowed by move, so a `serves` program launches through init
at most once per boot; a second launch of it is refused by name. Nothing
supervises or restarts a server: a namespace built before a server died
holds a connector to a port that is closed forever (§3), and only programs
init launches afterward can receive a fresh port.

## 7. Queueing and teardown

| server state | open | first write | first read |
|---|---|---|---|
| not yet accepting (not spawned, or not yet at accept) | succeeds; the connection queues | buffered | blocks |
| accepted, alive | succeeds | buffered | data |
| exited, before or after accepting | `Gone` | `Gone` | `0` |
| name not in the caller's namespace | `NotFound` | — | — |

No row is reached by waiting: there is no timer, no deadline, and no retry
anywhere in the protocol. A client already blocked in a read when its peer
dies is woken and observes `0` within one scheduler pass (§1.6).

A server never blocks on a client: a server write that cannot complete
drops that one peer, and an `Acceptor` has no write operation at all.

On teardown — exit and kill identically:

| last handle of this kind released | the peer observes |
|---|---|
| `Acceptor` | queued clients `Gone`/EOF; later opens `Gone` |
| connection end | `Gone` on write, `0` on read |
| `Connector` | nothing |
| `Namespace` | nothing |
| device claim | the class is claimable again |
| shared memory, last mapping included | nothing; the memory is gone |

## 8. Enforcement

The kernel enforces every handle, right, and bound above at the syscall
boundary. init enforces the manifest's routing: it alone reads the manifest,
and a program learns only what it was endowed. The image build enforces §4's
manifest rules, so a violating configuration never boots.
