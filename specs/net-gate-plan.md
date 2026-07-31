# Gate N — network testing strategy (planned)

Scheduled **after the first bare-metal attempt** (owner's call, 2026-07-31; see
`specs/metal-boot-plan.md`). This file records the strategy so it survives
until then. It deliberately mirrors gate A (audio) — same shape, same
discipline, one new capability the audio gate never had: the harness can be
the adversary.

Known risk of the scheduling: the netd idle→packet wake fix (drain_irqs NIC
fan-out) ships untested until this gate's first slice exists — nothing in tree
can deliver a frame to an idle guest. The idle-wake regression test below is
its missing regression coverage.

## What gate A proved worth copying

1. **Device-side ground truth.** The wav capture certified what reached the
   virtual hardware, not what the guest claimed. Net analogue: QEMU
   `-object filter-dump` writes a **pcap of the virtual wire**. The harness
   parses it offline: byte-exact payload verification, checksum/length
   validity of every frame ToyOS emits, ARP sanity, and retransmission rate
   as the harm detector (the analogue of gap detection).
2. **Two tiers.** Fast: every `cargo test`, one boot per config, per-run
   ceilings and structural assertions. Thorough: N iterations, distributions
   vs a recorded baseline (Mann-Whitney on counters, Fisher exact on yes/no
   outcomes), same-session A/B only.
3. **Daemon counters on serial.** netd analogue of soundd's stats line:
   rx/tx packets and bytes, retransmits, wakes, poll cycles, accepts —
   printed every ~2 s while active, parsed by the harness.
4. **A baseline file that justifies every number.** `tests/net-baseline.toml`
   with the same re-record protocol as `tests/audio-baseline.toml`
   (understood-and-justified only, never to make a red run green), the same
   host-suspend and TCG caveats.
5. **The instrument-defect lesson** (`specs/audio-gate-history.md`): the
   analyzer is code and it will be wrong first. Budget for certifying the
   instrument against known-good and known-bad captures before trusting it.

## Configs

- **slirp** (QEMU user networking): realistic TCP against real host sockets.
  Guest→host via 10.0.2.2 to a harness-owned listener; host→guest via
  `hostfwd` (to sshd and a guest echo server). Covers DHCP, DNS, TCP relay.
- **harness-as-peer** (`-netdev socket`/dgram): the harness owns the other
  end of the Ethernet link at frame level. This is the config gate A never
  had — the wire itself is programmable.

## Fast-tier certifications

- TCP echo both directions: pseudorandom seeded payloads, byte-exact, sizes
  1 B / 1 KiB / 1 MiB, several concurrent connections.
- UDP send/recv with documented loss semantics; DNS against slirp.
- Lifecycle: connect/close, RST, half-close, accept churn, reconnect storms.
- **Idle→packet wake ceiling**: harness waits for the guest to report full
  idle, sends one frame, asserts a response within a ceiling. This is the
  regression test for the drain_irqs NIC fan-out and the whole
  wake-loss bug class the suspend-series review found.
- **Adversarial frames** (harness-as-peer): truncated headers, wrong length
  fields, giant and zero-length frames, garbage — the kernel must return
  errors, never panic; netd may drop but must not wedge. The network is
  untrusted input; this is the trust-boundary principle applied to the wire.
- **Deterministic impairment** (harness-as-peer): seeded loss, reorder,
  delay, duplication — smoltcp retransmission behavior becomes reproducible,
  not statistical.
- pcap structural checks: no malformed frame ever emitted, retransmission
  rate ceiling, no packet storm at idle.

## Thorough tier / performance

Recorded distributions for throughput (bulk transfer), RTT, idle-wake
latency, counter rates. Under TCG these are relative numbers only (same
non-uniform-distortion argument as audio); the 2× production comparison is
honest only on metal. The T14's NIC is an Intel I219 — a new driver, not
virtio-net — so gate N running after metal can certify both NICs with the
same guest-side tests; the driver-facing analyzer does not care.

## Open questions for the implementation pass

- pcap parsing: minimal hand-rolled parser in the test crate vs a host-side
  dev-dependency — decide by what the checks actually need.
- What counter surface smoltcp already exposes vs what netd must count.
- Whether sshd joins the gate (a real protocol over the stack) or stays a
  separate test.
- Frame-injection format details for `-netdev socket` (length-prefixed
  stream) and whether dgram mode is simpler.
