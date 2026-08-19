---
status: open
kind: track
opened: 2026-07-31
---

# There is no network gate, and the idle-wake fix shipped untested behind that

Nothing in the tree can deliver a frame to an idle guest, so the netd
idle→packet wake fix shipped with no regression coverage at all. The gate was
scheduled after the first bare-metal attempt by the owner on 2026-07-31; that
attempt has since happened many times.

**It mirrors gate A deliberately**, because four things about gate A were worth
copying and one capability the audio gate never had is available here.

- **Device-side ground truth.** QEMU's `-object filter-dump` writes a pcap of
  the virtual wire; the harness parses it offline. Byte-exact payloads,
  checksum and length validity of every frame ToyOS emits, ARP sanity, and
  retransmission rate as the harm detector — the analogue of gap detection.
- **Two tiers.** Fast: one boot per config, per-run ceilings, structural
  assertions. Thorough: N iterations against a recorded baseline, Mann-Whitney
  on counters and Fisher exact on yes/no outcomes, same-session A/B only.
- **Daemon counters on serial**, netd's analogue of soundd's stats line.
- **A baseline file that justifies every number**, re-recorded only when the
  change is understood and justified, never to make a red run green.
- **The instrument is code and it will be wrong first.** Budget for certifying
  the analyser against known-good and known-bad captures before trusting it.

**The new capability is `-netdev socket`: the harness owns the other end of the
Ethernet link at frame level.** That makes three things possible that gate A
could not do — adversarial frames (truncated headers, wrong length fields, giant
and zero-length, garbage: the kernel must return errors and never panic, netd
may drop but must not wedge), deterministic impairment (seeded loss, reorder,
delay, duplication, so smoltcp's retransmission becomes reproducible rather than
statistical), and the **idle→packet wake ceiling** that is the actual regression
test for the shipped fix: wait for the guest to report full idle, send one
frame, assert a response inside a ceiling.

The second config is slirp, for realistic TCP against real host sockets — DHCP,
DNS, and a host↔guest relay both ways.

Under TCG the thorough tier's numbers are relative only; the 2× production
comparison is honest on metal alone. The T14's NIC is an Intel I219 rather than
virtio-net, so a gate landing after metal certifies both NICs with the same
guest-side tests — the analyser does not care which driver is underneath.

Open at the implementation pass: hand-rolled pcap parsing versus a host
dev-dependency; what counter surface smoltcp already exposes against what netd
must count; whether sshd joins the gate or stays separate.
