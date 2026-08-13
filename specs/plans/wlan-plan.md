# ToyOS WLAN — the Intel AX210 plan

The network path the owner chose (2026-08-03, overriding a wired-first
recommendation). The target is the one radio in the one real machine this
project has: `09:00.0`, `8086:2725`, "Wi-Fi 6E (802.11ax) AX210/AX1675* 2x2
[Typhoon Peak]", listed **undriven** in `specs/reference/metal-hardware-inventory.md:126`
and read off a boot that happened.

This file is the shape of the whole track. It is a spec and nothing in it is
built.

**Everything numeric here was measured on 2026-08-03** by downloading the
sources and counting them, against `torvalds/linux` at
`075b74841bd0065a3bda3440873c747938e69b68` and `openbsd/src` at
`ea7aa979ef7ad8461a888846635f92913f5de4d4`. Figures that are estimates say so in
the same sentence. The method for each is stated where the number appears, so a
later reader can re-run it rather than trust it.

---

## 1. Port someone else's driver, or write our own

This was the owner's live question and it is answered first because every other
section is downstream of it.

### 1.1 The frame that was checked, and the part of it that was wrong

The framing handed to this spec was: Intel publishes no register-level
documentation for this silicon, every non-Linux driver derives from Linux's
`iwlwifi`, so the choice is (a) port C, (b) transliterate to Rust using an
existing driver as documentation, or (c) hybrid per layer.

The premise is right about the PDF and wrong about the consequence. **Intel does
publish register-level documentation for this silicon — as C headers, in the
kernel tree it maintains.** `iwl-csr.h` (661 lines), `iwl-fh.h` (735) and
`iwl-prph.h` (548) are the control-and-status, flow-handler and peripheral
register maps; `fw/api/` (18,232 lines, **zero `.c` files**) is the complete
firmware command ABI as structs and enums. That is 21,533 lines of pure
declaration, and it is better documentation than a datasheet because it is the
artifact the vendor's own driver is compiled against, so it cannot drift from
the firmware without the vendor noticing.

`iwlwifi` is Intel's, not a community reverse-engineering effort:
`MAINTAINERS:13464` lists `INTEL WIRELESS WIFI LINK (iwlwifi)`, maintainer
`Miri Korenblit <miriam.rachel.korenblit@intel.com>`, status `Supported`, tree
`git.kernel.org/pub/scm/linux/kernel/git/iwlwifi/iwlwifi-next.git`.

That changes the question from "whose reverse engineering do we copy" to "which
layers of the vendor's own work do we consume, and in what form".

### 1.2 The licensing finding that reframes it further

A census of the first line of every `.c`/`.h` file in
`drivers/net/wireless/intel/iwlwifi/` (285 files, 185,608 lines):

| SPDX header | files |
|---|---|
| `GPL-2.0 OR BSD-3-Clause` | 238 |
| `GPL-2.0-only` | 46 (31,873 lines) |
| legacy comment block, no SPDX | 1 |

**The dominant licence is dual.** A recipient elects an arm; electing
BSD-3-Clause carries attribution obligations and no copyleft. So reading,
transcribing from, and shipping derivatives of 238 of those files is a matter of
reproducing a notice — not of licensing ToyOS's WLAN stack under the GPL.

An earlier arrangement in this project accepted a **GPL-contained** component
(the driver isolated so its licence did not reach the rest of the tree). The
owner accepted that. It is recorded here because it was a real decision, and it
is **superseded**: the BSD arm makes containment unnecessary, and a containment
boundary that exists for no reason is an abstraction that has not earned its
place.

### 1.3 The three options, priced

Corpus definitions used throughout, all measured:

- **AX210-relevant path** — the top level plus `cfg/`, `fw/`, `fw/api/`,
  `pcie/`, `pcie/gen1_2/`, `mvm/`, excluding test directories: **119,187 lines**.
  `mvm/` is the correct op_mode for this device: `pcie/drv.c:492` binds
  `0x2725` to `iwl_ty_mac_cfg` and `pcie/drv.c:1021` names it
  `iwl_ax210_name` with `iwl_rf_gf`. `dvm/` (27,612), `mld/` (34,107),
  `mei/` (4,147) are other devices or other subsystems and are out of scope.
- **BSD-arm subset of it** — after removing the 15 `GPL-2.0-only` files in that
  path (6,517 lines): **157 files, 112,670 lines**.

**(a) Port C — OpenBSD `iwx`.** `if_iwx.c` 12,884 + `if_iwxreg.h` 8,961 +
`if_iwxvar.h` 858 = **22,703 lines**, and it is compact and readable. It fails
on three independent counts:

1. It does not carry the feature the hardware was chosen for. OpenBSD's own
   `iwx(4)` manual states the driver does not support any of the 802.11ax
   capabilities the adapter offers. *(Read from the man page via search result,
   not from the driver source — flagged as unverified at this level of
   confidence.)*
2. It is not free-standing: it is written against OpenBSD's `net80211`, which is
   **31 files, 24,665 lines**, and which would arrive with it. That stack is
   BSD-licensed and good, and it is also 24,665 lines of C that would become
   ToyOS's permanent 802.11 layer — the opposite of the Rust-native principle,
   in the one area (WPA key handling, `ieee80211_pae_input.c`/`_output.c`,
   1,838 lines) where correctness is security.
3. It puts ToyOS one generation behind on a device whose firmware ABI moves.
   The shipped firmware for this part has gone 59 → 89 in API revision (§4.1);
   tracking Intel through OpenBSD adds a hop.

**(b) Transliterate everything to Rust.** The AX210 STA subset (§1.5) is
**75,383 lines** of imperative C to re-express. Nothing about that is impossible
and effort is never an argument here — but it discards the thing the owner
explicitly wants to keep: *"I don't want to rewrite Intel's software; if they
actively maintain those drivers and give patches I want to benefit."* Transliterating
`fw/api/` in particular is 18,232 lines of hand-copied constants whose only
correctness criterion is matching a blob we cannot inspect, re-done on every
firmware API bump. That is the worst possible thing to own by hand.

**(c) Hybrid per layer.** Requires a boundary that is real rather than
aesthetic. §1.4 says where it is and §1.5 says why there.

### 1.4 The decision

**Hybrid, and the boundary is declarative versus imperative.**

- **Tracked as C, in a fork** — everything that is a *declaration*: the firmware
  command ABI, the register maps, the device and RF configuration tables.
  **22,435 lines.** This is where Intel's churn lands, it is the part we most
  want their patches for, and it contains no control flow to be wrong about.
- **Ours, in Rust** — everything with control flow: the PCIe transport, the
  firmware load and alive handshake, the host-command path, the op_mode logic,
  and above it the entire 802.11 SME and WPA supplicant.

The fork is a real ToyOS fork under the existing discipline: own repository, a
`toyos` branch off a pinned upstream base, minimal delta, periodic upstream
merges, an entry in `forks.toml`. `git log <base>..toyos` stays exactly the
ToyOS delta, as for every other fork. **This extends the fork discipline to C
for the first time**, and `forks.toml`'s tier vocabulary gains a case: this one
is neither `sibling` nor `fork` in the existing sense — it is upstream code
consumed as an interface definition, with a delta that should approach zero.

**Pin source: `torvalds/linux`, not `iwlwifi-next`.** Both are Intel's work;
mainline is tagged, bisectable, and has a stable notion of "released", which is
what a pinned base needs. `iwlwifi-next` is a staging tree whose history is
rewritten. Revisit only if a firmware API revision we need lands there and stalls
before mainline.

### 1.5 Why the boundary is there and not somewhere else

Because it is where the cost cliff is, and the cliff was measured. Counting the
distinct `#include <linux/...>` and `<net/...>` headers each area needs:

| Area | BSD-arm lines | distinct kernel headers |
|---|---|---|
| `fw/api/` | 18,232 | **6** |
| `cfg/` | 1,357 | **2** |
| `pcie/` | 2,932 | 5 |
| `fw/` | 12,456 | 16 |
| top level | 13,101 | 32 |
| `pcie/gen1_2/` | 13,000 | (within `pcie`'s set) |
| `mvm/` | 51,592 | **44** |

The AX210-relevant subset as a whole needs **75 distinct Linux kernel headers**
(65 `linux/*`, 10 `net/*`) — `skbuff.h`, `dma-mapping.h`, `dmapool.h`, `pci.h`,
`workqueue.h`, `spinlock.h`, `slab.h`, `netdevice.h`, `firmware.h`, `rcupdate.h`
by way of `cleanup.h` and `lockdep.h`, plus `thermal.h`, `ptp_clock_kernel.h`,
`efi.h`, `acpi.h`, `dmi.h`, `leds.h`. Supplying those is supplying a Linux kernel
emulation layer. FreeBSD built exactly that (LinuxKPI) to host this driver, which
is proof it is possible and proof of its size.

The density behind those headers, counted over the same 157 files:

| construct | occurrences |
|---|---|
| `BIT` | 1,459 |
| `skb` | 767 |
| `__packed` | 635 |
| `rcu_` | 342 |
| `dma_` | 285 |
| `kfree` | 204 |
| `spin_lock` | 141 |
| `BUILD_BUG_ON` | 125 |
| `likely` | 116 |
| `kzalloc` | 71 |
| `container_of` | 49 |
| `typeof` | 22 |

**`fw/api/` and `cfg/` sit on the cheap side of the cliff by an order of
magnitude** — 6 and 2 headers, all trivial (`linux/types.h`, `bits.h`,
`bitops.h`, `bitfield.h`, `if_ether.h`, `ieee80211.h`), every one of which ToyOS
writes for itself in a few hundred lines. Everything else needs the LinuxKPI.
The boundary is not a matter of taste; it is the point where the environment cost
multiplies by seven.

**The declarative fork set**, then:

| Content | lines |
|---|---|
| `fw/api/` — firmware command ABI, 35 headers, no `.c` | 18,232 |
| `cfg/` — device and RF tables (`ax210.c` is 143 lines) | 1,357 |
| `iwl-config.h` 759, `iwl-fh.h` 735, `iwl-csr.h` 661, `iwl-prph.h` 548, `iwl-scd.h` 84, `iwl-agn-hw.h` 59 | 2,846 |
| **total** | **22,435** |

**The Rust side**, for a station-only client:

| Content | C lines it replaces |
|---|---|
| top level minus the six headers above | 10,255 |
| `fw/` — firmware load, host commands, debug TLVs | 12,456 |
| `pcie/` + `pcie/gen1_2/` — transport, TFD/RB rings | 15,932 |
| `mvm/` station core (§1.6) | 36,740 |
| **total** | **75,383** |

22,435 + 75,383 + 14,852 (the `mvm/` features a station client does not need)
= 112,670, which closes against the measured BSD-arm subset exactly.

### 1.6 What "station core" excludes, by file

`mvm/`'s BSD-arm content is 51,592 lines. Excluding the features a client
station does not use leaves **36,740**. The excluded 14,852, named so the
judgment is checkable rather than asserted: `d3.c` 3,300 (WoWLAN),
`debugfs.c` 2,201 + `debugfs-vif.c` 911 + `debugfs.h` 44, the four `mld-*.c`
2,561 (multi-link, 802.11be), `ftm-initiator.c` 1,484 + `ftm-responder.c` 440
(ranging), `tt.c` 864 (thermal), `coex.c` 714 (Bluetooth coexistence),
`tdls.c` 664, `ptp.c` 338 + `time-sync.c` 173 + `time-sync.h` 30, `sf.c` 288,
`quota.c` 258, `offloading.c` 214, `rfi.c` 157, `led.c` 119, `testmode.h` 92.

Two of those exclusions are provisional and the spec says so rather than
discovering it later: **`tt.c` (thermal throttling)** is a laptop part in a
thin chassis, and **the four `mld-*.c`** may be on the AX210 path depending on
which firmware API revision negotiates multi-link. Both are checked at W7, not
assumed.

### 1.7 What would reopen this decision

One number: **if the declarative fork's delta stops approaching zero.** The
whole argument is that `fw/api/` and `cfg/` can be tracked with a delta small
enough that an upstream merge is mechanical. If the first two upstream merges
require more than trivial conflict resolution, the fork is not paying for
itself and transliterating those 22,435 lines becomes the better answer.
Recorded as W6's exit criterion so the question is asked with data.

---

## 2. Licensing, precisely

### 2.1 What we take and what we publish

- Dual-licensed iwlwifi files are taken under **BSD-3-Clause**.
- ToyOS's own components — the Rust transport, op_mode, SME, WPA — are
  **MIT OR Apache-2.0**, matching the rest of userland.
- Intel's BSD-3-Clause notice is **reproduced for anything materially
  transcribed or carried**. The fork repository carries it because it carries
  the files; anything lifted into the Rust side carries it at the site.
- The GPL-contained arrangement the owner previously accepted is superseded
  (§1.2). No part of this plan needs it.

### 2.2 The GPL-2.0-only exclusion, and the rule

**Adopted rule: `GPL-2.0-only` files are never read.** Not read, not consulted,
not transcribed. Anything they turn out to gate is written from the IEEE 802.11
standard instead.

The 15 such files inside the AX210 path, by name (6,517 lines):

```
iwl-debug.h              iwl-devtrace.c           iwl-devtrace.h
iwl-devtrace-data.h      iwl-devtrace-io.h        iwl-devtrace-iwlwifi.h
iwl-devtrace-msg.h       iwl-devtrace-ucode.h
cfg/1000.c  cfg/2000.c  cfg/5000.c  cfg/6000.c
mvm/rs.c  mvm/rs.h  mvm/vendor-cmd.c
```

What each costs, per item, rather than as a blanket statement:

- `iwl-devtrace*` (7 files) and `iwl-debug.h` — ftrace and logging macros.
  ToyOS has its own log ring; nothing is lost.
- `cfg/{1000,2000,5000,6000}.c` — configuration for 2009-era devices. Not this
  machine.
- `mvm/vendor-cmd.c` — Intel's nl80211 vendor commands. ToyOS has no nl80211.
- **`mvm/rs.c` + `mvm/rs.h` (4,635 lines) — legacy rate scaling, and this one
  genuinely gates something.** Rate selection comes instead from
  **firmware-driven rate scaling**: `mvm/rs-fw.c` (740 lines) is
  `GPL-2.0 OR BSD-3-Clause` and is the path where the firmware owns the rate
  decision. That is the correct path for this device regardless of licensing,
  and the rule costs nothing here. If a case appears where host-side rate
  selection is needed, it is written from the standard.

The rule reaches further than iwlwifi, and the spec states it because it is
load-bearing for §3.3: **`net/mac80211.h` (8,206 lines), `net/cfg80211.h`
(10,995) and `linux/ieee80211.h` (2,891) are all `GPL-2.0-only`.** The interface
we must implement is defined in headers we do not read. §3.3 says how the
surface was measured without them, and how it must be implemented.

`mac80211_hwsim` — the obvious answer to the harness problem — is
`GPL-2.0-only` across all five of its files (9,595 lines: `_main.c` 7,631,
`_nan.c` 1,346, `.h` 348, `_i.h` 168, `_nan.h` 102). Excluded. §5 answers the
harness problem without it.

### 2.3 Firmware redistribution

`LICENCE.iwlwifi_firmware`, Copyright 2006–2021 Intel Corporation, read in full:

- Redistribution **in binary form, without modification, is permitted**, provided
  the copyright notice and the disclaimer are reproduced in the documentation
  and/or other materials provided with the distribution.
- Intel's name may not be used to endorse derived products without written
  consent.
- **Reverse engineering, decompilation and disassembly are prohibited.**
- The patent grant is worldwide and royalty-free, scoped to using the software
  alone or with an OS under an OSI-approved licence. MIT and Apache-2.0 are
  both OSI-approved, so ToyOS is inside the grant.

**ToyOS ships the blob.** OpenBSD does not, and that is a project policy about
the terms it is willing to accept, not evidence that redistribution is
forbidden — the two are routinely conflated and the licence text settles it.

The prohibition on disassembly is worth naming as a constraint on debugging: when
the firmware misbehaves, the available evidence is the host-command transcript
and the firmware's own error notifications, never a disassembly. §5 builds the
transcript recorder partly for this reason.

### 2.4 Notice placement

The notice lives at **`LICENSES/LICENCE.iwlwifi_firmware` on the boot volume**,
mirroring linux-firmware's own layout (the file is at that exact path in the
upstream repository), with the blobs alongside the other firmware ToyOS will
accumulate. This aligns with task #98's initrd-shrink direction: firmware is
boot-volume content, not initrd content, because nothing needs it before the
filesystem is up.

```
/boot/LICENSES/LICENCE.iwlwifi_firmware
/boot/firmware/iwlwifi-ty-a0-gf-a0-89.ucode
/boot/firmware/iwlwifi-ty-a0-gf-a0.pnvm
```

W4's gate asserts the notice file is present in the built image. A firmware blob
that ships without its notice is a licence violation that no test would otherwise
catch.

---

## 3. Architecture

### 3.1 Where it lives

**Userland, under netd, IOMMU-confined**, per `specs/plans/userspace-drivers-spec.md`.
The kernel stays Rust-pure and gains nothing for WLAN.

This is a device *entering* userland rather than *leaving* the kernel, so the
rule that "no driver leaves the kernel before the IOMMU is complete"
(`userspace-drivers-spec.md` §2) applies in its stronger form: there is no
kernel-resident interim driver at any point. The prerequisites are IOMMU stages
**I2** (translation on), **I3** (interrupt remapping) and **I4** (domains,
mapping, invalidation, faults), plus `userspace-drivers-spec.md` stages **3**
(BAR sizing and 2 MiB re-assignment) and **4** (the capability). I0 and I1 are
done.

Task #88 (HDA audio) needs the same prerequisites, so the path is shared and
neither track pays for it alone.

**The early checkpoint that could invalidate all of this is W0.** Two rules in
the IOMMU spec can refuse this specific device for userspace handoff:

- `iommu-spec.md` §7.3 — a device is handed to userspace only if its isolation
  scope is a singleton. `09:00.0` sits behind PCI bridge `00:1c.x`; whether that
  bridge implements ACS on this machine is unknown.
- `iommu-spec.md` §7.4 — a device carrying an RMRR is refused. QEMU publishes
  none, so this has never been exercised, and the T14 is the first machine that
  can answer it.

If either refuses, WLAN under this architecture does not happen and the plan
needs re-deciding rather than patching. W0 exists to learn that before any
driver line is written.

### 3.2 Process shape

**One process** under netd. Daemon-level crash isolation already exists — a
crashed WLAN daemon does not take the kernel or netd's peers with it — and there
is no evidence yet that an IPC boundary *inside* the stack buys anything. A
framework before there are three users is an abstraction with no evidence behind
it (`userspace-drivers-spec.md` §8.7, same argument).

The criteria that would trigger revisiting, recorded now so the decision is
falsifiable later:

- The 4-way handshake or the assoc path misses a timeout because the transport's
  interrupt work starves it. That is a scheduling argument for a separate
  process, and it would show up as a measurable retry rate.
- The supplicant needs to hold key material the driver half has no business
  reading. Today it does not, because a compromised driver half owns the radio
  anyway.
- A second radio appears. Two devices in one process share a failure domain for
  no reason.

### 3.3 The interface surface, measured

This is the spec's central sizing number, because it is exactly what ToyOS has
to build for the fork's declarations to be usable and for a Rust op_mode to have
something to sit in.

**Method**, stated because it had to route around §2.2's rule: every
`ieee80211_*`, `cfg80211_*`, `wiphy_*` and `regulatory_*` call site was extracted
from the **157 BSD-arm files** — code we are licensed to read — and the resulting
identifiers were classified against the upstream headers used purely as an index
of names. Names and interfaces are facts; the implementations were not read and
must be written from the call sites and from the 802.11 standard.

| Surface | count |
|---|---|
| Distinct functions iwlwifi **calls** (86 from `mac80211.h`, 34 from `cfg80211.h`, 4 in both) | **116** |
| `ieee80211_ops` callbacks iwlwifi **implements**, which we must call (`mvm/mac80211.c:6352`) | **73** |
| Struct types referenced, of which 58 are API types we must define and 4 are protocol structs | **79** |
| Constants referenced (470 `IEEE80211_*`, 143 `NL80211_*`, 45 `WLAN_*`) | **658** |

**116 + 73 + 58 is the shim.** It is a real number and it is far smaller than the
151,395 lines of `mac80211` (94,233) plus `cfg80211` (57,162) that upstream puts
behind it, because most of that mass is features iwlwifi never calls into and
modes we do not implement.

The 116 do not cost the same and the staging depends on the difference:

| Bucket | ≈count | What it is |
|---|---|---|
| Pure protocol helpers | 22 | `ieee80211_hdrlen`, `ieee80211_data_to_8023`, `cfg80211_find_ie`, `ieee80211_channel_to_frequency`, … — functions over 802.11 frames and channels that W1 writes from the standard anyway. Near-zero marginal cost. |
| Lifecycle and registration | 16 | `ieee80211_alloc_hw`/`register_hw`/`free_hw`, `hw_set`/`hw_check`, `wiphy_rfkill_*`, `regulatory_set_wiphy_regd`. Small and mechanical. |
| Deferred work and locking | 8 | `wiphy_work_init`/`queue`/`flush`/`cancel`, `wiphy_lock`/`unlock`. Maps onto ToyOS's own event loop; this is where an impedance mismatch would show. |
| Data path | 8 | `ieee80211_rx_napi`, `ieee80211_tx_dequeue`, `ieee80211_tx_status_ext`, `ieee80211_stop_queues`/`wake_queues`. Small in count, most of the runtime. |
| MLME upcalls | 20 | `ieee80211_connection_loss`, `ieee80211_beacon_loss`, `ieee80211_scan_completed`, the five `iterate_*`, `cfg80211_bss_iter`. **These are the SME's inputs** — W3 is the thing that answers them. |
| Aggregation | 6 | BlockAck session callbacks. |
| Key management | 11 | `get`/`set_key_rx_seq`, `key_mic_failure`, `key_replay`, `gtk_rekey_*`. Four are TKIP and are not needed (CCMP only). |
| Not needed for a station | ~23 | Beacon countdown/CSA, remain-on-channel, WoWLAN, TDLS, PMSR, uAPSD buffering, MU-MIMO groups. Refuse by name rather than stub silently. |

Of the **73 callbacks**, roughly 40 are required for a station and roughly 33
belong to modes we do not implement (`start_ap`, `stop_ap`, `join_ibss`,
`leave_ibss`, three `tdls_*`, the channel-switch family, `start_pmsr`,
`abort_pmsr`, `suspend`, `resume`, `set_wakeup`, the `*_debugfs` trio,
`sched_scan_start`/`stop`). The split is this spec's classification, applied by
the rule *"does a client station associating to an AP reach this?"*, and it is
the first thing W7 should check against reality.

### 3.4 Crates

| Crate | Kind | Content |
|---|---|---|
| `toyos-80211` | `no_std`, host-tested | Frame parse/build, IE parser, channel and regulatory *representation* (not policy), the 658-constant protocol surface from the standard. |
| `toyos-wpa` | `no_std`, host-tested | PBKDF2-HMAC-SHA1, PTK/GTK derivation, the 4-way handshake state machine, CCMP, replay windows, key lifecycle. **Correctness here is security**; it is the crate with the most test mass per line. |
| `toyos-sme` | `no_std`, host-tested | Scan, authenticate, associate, roam and disconnect state machines. Consumes `toyos-80211`, drives `toyos-wpa`. |
| `iwlwifi-abi` | fork consumer | Rust `#[repr(C)]` mirrors of the fork's declarations, plus per-struct layout assertions (§6, W6). |
| `userland/wland` | binary | The transport, the op_mode, and the surface of §3.3. One process, under netd. |

The first three are `no_std` and host-testable **on purpose**: they are the part
of this stack where ground truth is available without hardware, and §5 leans on
that entirely.

---

## 4. Firmware

### 4.1 Identity and version pin

Verified against the upstream sources, not inferred:

- `cfg/ax210.c:19` — `#define IWL_TY_A_GF_A_FW_PRE "iwlwifi-ty-a0-gf-a0"`.
  `ty` is Typhoon Peak (the `iwl_ty_mac_cfg` bound to `0x2725`), `gf` is the
  Garfield RF module (`iwl_rf_gf`).
- `cfg/ax210.c:13,16` — `IWL_AX210_UCODE_API_MAX` and `..._MIN` are both **89**.
  A single supported revision, not a range.
- `iwl-config.h:106` — `IWL_FW_AND_PNVM` expands to *two* required files: the
  `.ucode` and a `.pnvm`.

The two files, confirmed present in linux-firmware at `intel/iwlwifi/`, with
sizes read from the server:

| File | bytes |
|---|---|
| `iwlwifi-ty-a0-gf-a0-89.ucode` | 1,678,860 |
| `iwlwifi-ty-a0-gf-a0.pnvm` | 55,020 |
| **total** | **1,733,880** (1.65 MiB) |

The same directory carries thirteen revisions (59, 66, 72, 73, 74, 77, 78, 79,
81, 83, 84, 86, 89), which is the churn rate §1.4 is designed around.

**The pin is exact and it is a build-time constant**: the driver refuses any
blob whose TLV-declared API revision is not the one it was built against. A
firmware whose ABI moved under us is not a degraded mode, it is a different
device, and this project does not do fallbacks.

### 4.2 Blob in git, or fetched at build time

**In git, in the fork's own repository** — not in the monorepo, and not fetched.

Reasoning, in the order that decided it:

1. **`cargo run` must work from a fresh clone with no setup.** That is a stated
   property of this project's fork estate. A build-time fetch makes the network
   a build dependency and makes an offline build impossible.
2. **1.65 MiB is affordable.** The boot partition is 35,651,584 bytes; the
   console initrd alone is 25,165,824. The blob is under 5% of the volume it
   sits on.
3. **A blob is content, not code, and its licence forbids modification** — so it
   is exactly the kind of artifact a pinned repository is good at: it never
   diffs, it only gets replaced wholesale, and its hash is its identity.
4. Putting it in the *fork's* repository rather than the monorepo keeps the
   monorepo free of binaries and puts the blob next to the declarations that
   describe it, so a firmware API bump is one coordinated commit in one place.

The build copies both files and the notice into the image. The image build
asserts the blob's SHA-256 against a constant, so a corrupted or substituted
blob fails the build rather than the radio.

### 4.3 Upload

The `.ucode` is a TLV container: a header, then typed sections (instruction
memory, data memory, the alive/paging sections, capability and API-flag TLVs
that declare what this firmware supports). The driver parses it, validates the
API revision against its pin, DMA-maps the sections, writes them through the
context-information structure the AX210 generation uses
(`pcie/ctxt-info-v2.c`, 618 lines — the gen2 path this device takes), releases
the CPU, and waits for the ALIVE notification. The `.pnvm` is a second image
uploaded after alive.

All of that is described by the fork's declarations. It is W7's work and its
first real gate is a machine that answers.

**Every DMA buffer here goes through the IOMMU capability's `SYS_DMA_MAP`.**
A 1.6 MiB firmware image is the largest single mapping this driver makes, and at
the kernel's 2 MiB granularity (`iommu-spec.md` §5.4) it is one page. That is a
convenient accident and it should not be relied on: the `.ucode` grows.

### 4.4 Regulatory

**The firmware's answer is authoritative. ToyOS carries no regulatory table.**

The device's NVM and the firmware's MCC (mobile country code) machinery decide
which channels are legal at what power. The driver reads that and obeys it; it
never widens it, and there is no override. This is the correct engineering answer
as well as the correct legal posture: the alternative is ToyOS shipping a table
that claims to know the radio rules of every jurisdiction, which is a claim it
cannot back.

Consequence to state rather than discover: the channel list is not known until
the firmware is alive, so scanning cannot be configured at compile time, and a
regulatory change mid-session (from a beacon's country IE) can remove a channel
the station is on.

---

## 5. The harness problem

QEMU emulates no Intel Wi-Fi device. There is no `-device` that presents
`8086:2725`, and nothing in the harness can be made to speak this firmware's
host-command protocol. This section is the honest answer, priced, because a
stage whose only verification is "it worked when the owner tried it" must say so
in advance.

### 5.1 Four levels of ground truth

**L1 — host tests of the protocol cores.** `toyos-80211`, `toyos-wpa`,
`toyos-sme` are `no_std` crates with no device dependency. They are tested the
way `toyos-sched`, `toyos-gpt` and `toyos-fat32` are: `cargo test` inside the
crate, on the host, deterministically. WPA gets published test vectors (IEEE
802.11's own, and the CCM vectors in RFC 3610); the SME gets a scripted peer.
**Cost: zero beyond writing the crates. Coverage: complete, for the layers where
correctness is security.** This is the single most valuable fact in this section
and it is why the stage order puts these first.

**L2 — recorded-transcript replay.** The driver records every host command it
sends and every notification it receives, with timestamps, into the kernel log
on a real boot. The harness replays a recorded transcript against the driver's
op_mode with the transport stubbed, and asserts the driver produces the same
command sequence. **Cost estimate: ~800–1,500 lines of harness plus a recording
hook in the driver — an estimate, not a measurement.** **Coverage: regressions
only.** It certifies that a change did not alter behaviour that once worked. It
certifies nothing about new behaviour, and a transcript recorded from a buggy
run enshrines the bug. That limitation is exactly the instrument-defect lesson
from `specs/assessments/audio-gate-history.md` and it applies here with full force.

**L3 — a synthetic 802.11 device model.** The thing `mac80211_hwsim` is in
Linux, and unavailable to us both because it is `GPL-2.0-only` (§2.2) and
because it simulates the *mac80211 side*, not an AX210. A model faithful enough
to catch real driver bugs would have to implement the firmware's host-command
protocol — 18,232 lines of command definitions in `fw/api/` — which is to say it
would be a re-implementation of the firmware we are not allowed to disassemble.
An unfaithful model is worse than none: it is a green gate that proves only that
the driver agrees with our guess, which is precisely the vacuity trap
`userspace-drivers-spec.md` §7.2 is built around. **Estimated cost of a partial
model covering init, scan and assoc: 6,000–10,000 lines — an estimate.**
**Recommendation: do not build it.** The money is better spent on L2 plus more
of L1.

**L4 — the metal loop.** Flash the stick, boot the T14, the stick comes back
with `/log/kernel.log`. The pipeline exists (`specs/assessments/metal-log-capture.md`,
`--diag-boot`, `--console-boot`, the `TOYOS-LOG` partition macOS auto-mounts).
**Cost: the owner's time, per iteration.** **Coverage: the only ground truth
that exists for anything below the SME.**

### 5.2 Per-stage verification, and which stages carry the declared status

| Stage | Ground truth | Declared status |
|---|---|---|
| W0 handoff checkpoint | L4 | metal only — that is the point of it |
| W1 `toyos-80211` | L1 | fully verified in the harness |
| W2 `toyos-wpa` | L1 + published vectors | fully verified in the harness |
| W3 `toyos-sme` | L1 + scripted peer | fully verified in the harness |
| W4 firmware pipeline | L1 (parse the real blob) + image assertion | fully verified in the harness |
| W6 `iwlwifi-abi` | L1 (layout assertions) | fully verified in the harness |
| W7 transport + firmware load | L4, then L2 for regression | **verified only on the owner's boot** |
| W8 op_mode | L4, then L2 for regression | **verified only on the owner's boot** |
| W9 netd integration | gate N's stack gates (L1/QEMU) above; L4 below | split — see §6 |
| W10 WPA3-SAE | L1 + published vectors | fully verified in the harness |

**W7 and W8 are the two stages with no harness answer**, and they are the two
largest. That is stated here, in advance, as the plan's most uncomfortable
property. It is not fixable by cleverness — it is what "the machine has a device
nothing can emulate" means.

The mitigation is structural rather than procedural: **W1–W4 and W6 push as much
correctness as possible into the layers that *can* be tested**, so that when W7
and W8 fail on metal, the failure is in the transport or the firmware protocol
and not in frame parsing, key derivation, or the state machine. That is the same
move `specs/device-test-strategy.md` makes for storage, and the same one that
made the scheduler cutover survivable.

---

## 6. Stages

Every stage leaves the tree green: `cargo run -- --build-only` clean and
`cargo test` green including gate A's fast tier. Sizes are **estimates** except
where they restate a measured C line count.

| Stage | Content | Size (est.) | Gate |
|---|---|---|---|
| **W0** | **Handoff feasibility.** No driver. A kernel diagnostic that, for `09:00.0`, reports: its DMAR device scope, whether its isolation scope is a singleton (§3.1), whether it carries an RMRR, whether it offers MSI-X and how many vectors, and its BAR sizes and 2 MiB relocatability. Read on a `--diag-boot`. | ~200 lines kernel | The log carries a named line per item, photographed off the panel. **If scope or RMRR refuses the device, the track stops here and is re-decided.** |
| **W1** | **`toyos-80211`.** Frame parse/build for management, control and data; the IE parser; channel/frequency maps; the 658-constant protocol surface written from the standard. | ~3,500 lines Rust | `cargo test` in-crate. **State-space attacks, not just teeth**: truncated frames, an IE whose length runs past the buffer, a zero-length IE, duplicate IEs, an element chain that never terminates. Per `specs/metal-track-history.md`, mutating the parser tests the paths written; these test the states not thought of. |
| **W2** | **`toyos-wpa`, WPA2-PSK.** PBKDF2-HMAC-SHA1, PMK→PTK/GTK derivation, the 4-way handshake state machine, CCMP encrypt/decrypt, replay windows, key install and rekey. | ~2,500 lines Rust | `cargo test` against published vectors. Negative gates with teeth: a tampered MIC must fail, a replayed message 3 must not re-install, an out-of-window packet number must be dropped. **A green run with the MIC check deleted must go red**, or the gate is decoration. |
| **W3** | **`toyos-sme`.** Scan, auth, assoc, disconnect, roam; the 20 MLME upcalls of §3.3 are its inputs. | ~2,500 lines Rust | `cargo test` with a scripted peer: assoc timeout, deauth mid-handshake, a beacon that changes the country IE, an AP that never answers. |
| **W4** | **Firmware pipeline.** TLV container parse, API-revision pin, `.pnvm` handling, blob + notice into the image at §2.4's paths, SHA-256 assertion. | ~800 lines Rust + build | A test parses the real 1,678,860-byte blob and asserts its section inventory and API revision 89; a test asserts `LICENSES/LICENCE.iwlwifi_firmware` is in the built image. **Independently valuable — HDA and every later firmware device reuse it.** |
| **W5** | **Prerequisites land.** Not WLAN work. IOMMU I2–I4, `userspace-drivers-spec.md` stages 3–4. Shared with task #88. | — | Those specs' own exit criteria |
| **W6** | **`iwlwifi-abi`.** The C fork (§1.4, 22,435 lines) as its own repository with a `toyos` branch off a pinned mainline base, in `forks.toml`; a build step that emits Rust `#[repr(C)]` mirrors and constants. | ~1,200 lines tooling; fork delta target ≈ 0 | Per-struct layout assertions: `size_of` and every field offset checked against the C side. **A firmware-API bump that moves a field fails the build rather than corrupting a command.** Exit criterion also answers §1.7: record how much conflict resolution the first upstream merge needed. |
| **W7** | **Transport and firmware load.** Claim `09:00.0` through the device capability, map the BAR, set up TFD/RB rings, MSI-X, upload `.ucode` and `.pnvm`, reach ALIVE. Checks §1.6's two provisional exclusions and §3.3's 40/33 callback split against reality. | ~6,000 lines Rust (replaces 15,932 + 12,456 lines of C) | **L4 — metal only.** Declared: verified only on the owner's boot. L2 transcript recording lands with this stage so W8 has a regression net. |
| **W8** | **Op_mode.** Host-command path, NVM and regulatory read (firmware authoritative), scan, PHY/MAC contexts, station add/assoc, key upload, TX/RX, firmware rate scaling. | ~12,000 lines Rust (replaces 36,740 lines of C) | **L4 — metal only.** Declared status. L2 replay as the regression gate once a good transcript exists. |
| **W9** | **netd integration.** One process (§3.2), Ethernet-framed into smoltcp, association driven by the SME. | ~1,000 lines Rust | Gate N's stack gates run unchanged above it (§7); association and the 4-way handshake are metal-only. |
| **W10** | **WPA3-SAE.** The named later stage. Hash-to-curve on P-256, the SAE commit/confirm exchange, PMKSA caching, and the transition-mode interaction with W2. | ~2,500 lines Rust + constant-time EC | `cargo test` against published vectors; constant-time review is part of the gate, not a follow-up. |

W1–W4 are independent of everything else in this plan and of each other. They
can be built before W5's prerequisites exist, they are fully harness-verified,
and each is useful on its own — `toyos-wpa` in particular is the crate a future
Ethernet-with-802.1X or any other supplicant would reuse.

---

## 7. How gate N composes above this

`specs/plans/net-gate-plan.md` tests the stack — TCP echo, lifecycle, adversarial
frames, impairment, idle→packet wake — against a NIC, and it is deliberately
written so the driver-facing analyzer does not care which NIC.

That property holds here and it is worth being explicit about why: gate N's
assertions are about smoltcp's behaviour, netd's counters, and frames on a wire.
**Everything gate N certifies stays valid above a WLAN driver, and none of it
can be run against one in QEMU.** So the composition is:

- **Gate N's fast tier keeps running against virtio-net in the harness**, exactly
  as planned, and remains the regression gate for the stack.
- **On metal, gate N's guest-side tests run over WLAN** once W9 lands, with the
  harness-as-peer configs unavailable (there is no programmable wire) and the
  slirp-equivalent replaced by a real access point.
- The pcap ground truth that makes gate N honest has no WLAN analogue in the
  harness. On metal it would need a second machine capturing in monitor mode,
  which is out of scope here and is named so nobody assumes it.

`specs/reference/metal-hardware-inventory.md` names the I219-V Ethernet at `00:1f.6` as
gate N's metal target. The owner's sequencing keeps it: **the I219 is built
later — after doom and after WLAN's early stages — as gate N's metal enabler and
as a fallback NIC.** WLAN's stages therefore do not have to carry gate N's whole
metal story alone, and W9's gate is correspondingly narrower.

---

## 8. Sequencing

**Doom first** (owner). This spec is the only WLAN work now; implementation
dispatches stage by stage after doom's milestone completes and the IOMMU
prerequisites exist.

The dependency order that matters:

```
W0 ──────────────────────────────────► (gate: may stop the track)
W1 ─► W2 ─► W3                          independent of everything below
W4                                      independent
W5 (IOMMU I2–I4, usdrv 3–4)  ─► W6 ─► W7 ─► W8 ─► W9 ─► W10
```

W0 should be run early and cheaply — it is a diagnostic on a boot the owner is
already doing — because it is the only thing in this plan that can invalidate
the architecture rather than merely cost time.

---

## 9. Explicitly not doing

1. **AP, IBSS, mesh, P2P or TDLS modes.** Client station only. The ~33 callbacks
   and ~23 upcalls of §3.3 that serve them are refused by name, not stubbed
   silently.
2. **WoWLAN and suspend/resume of the radio.** `mvm/d3.c`'s 3,300 lines have no
   counterpart here.
3. **802.11be / multi-link.** The `mld/` op_mode (34,107 lines) is for other
   silicon; the `mvm/mld-*.c` files are checked at W7 and excluded until proven
   necessary.
4. **FTM / 802.11mc ranging, PTP, time sync.** No caller.
5. **Bluetooth.** The AX210's BT radio is a separate USB function
   (`8087:0032`, on the PCH xHCI per the inventory) and is not this plan's.
   Coexistence is left to the firmware; `mvm/coex.c` is excluded and that
   exclusion is provisional in the same sense as §1.6's two others.
6. **A regulatory database.** §4.4.
7. **Host-side rate scaling.** §2.2 — the firmware owns it.
8. **A synthetic 802.11 device model.** §5.1 L3, with the reasoning.
9. **Any kernel-resident WLAN code.** Not even temporarily, not even to get
   something working before the IOMMU lands.
10. **Disassembling the firmware.** §2.3 forbids it and the debugging strategy is
    built around not needing it.
11. **A generic "wireless framework".** There is one radio. A second one may
    share a crate with it.

---

## 10. Where I think this is wrong

- **The declarative/imperative boundary is clean in principle and will leak.**
  `fw/api/` is declarations, but `mvm/`'s use of them encodes assumptions about
  command ordering and about which fields the firmware reads — knowledge that
  lives in the imperative half we are re-writing. Some of that knowledge will be
  discovered on metal, at W8, expensively. The boundary is still right; the claim
  that it is free is not.
- **§3.3's 116/73/58 is a count of an interface, not of the work behind it.**
  `ieee80211_rx_napi` is one function and it is most of a receive path. Nobody
  should read that table as "the shim is 247 items of equal size", and the bucket
  breakdown exists to stop exactly that misreading.
- **The 40/33 station-callback split is my classification, not a measurement.**
  It was applied by one rule and it will be wrong somewhere. W7 checks it.
- **W7 and W8 together are ~18,000 lines of estimated Rust with no harness.**
  That is the largest untested-until-metal body of work this project has
  attempted. `specs/metal-track-history.md` records ~70 defects found in code
  whose own suites were green — and these two stages will not even have suites.
  If the plan fails, it fails here.
- **`toyos-cc` cannot process the fork's headers today, and the failure is
  silent.** Measured: `toyos-cc/include/compat.h` contains
  `#define __attribute__(x)` — it *strips* attributes. There are **635**
  `__packed` uses in the AX210-relevant subset, every one on a firmware ABI
  struct whose layout must be exact. Stripping them produces structs with wrong
  offsets that compile cleanly and misalign every firmware command. The compiler
  otherwise has what is needed (full bitfield layout and read-modify-write in
  `toyos-cc/src/types.rs` and `codegen/bitfield.rs`, anonymous struct/union,
  `typeof`, `__extension__`), so the gap is narrow — but "toyos-cc is not meant
  to grow" collides with W6 here, and the collision is real. W6's layout
  assertions catch the failure either way, which is the ladder applied honestly:
  this cannot be made unrepresentable, so it is checked at build time rather
  than hoped for.
- **The one-process decision (§3.2) is the weakest of the settled decisions.**
  It is right on present evidence and the evidence is thin — there is no
  measurement, because there is nothing to measure yet.

---

## 11. Open risks

- **`09:00.0` may not be handoff-able at all.** Isolation scope (`iommu-spec.md`
  §7.3) and RMRR (§7.4) are modelled, never measured, and QEMU cannot stage
  either. W0 is the first real answer and it can end this track.
- **OpenBSD's `iwx` not supporting 802.11ax is unverified** at the level this
  project requires — read from a manual page via a search result, not from the
  driver. It is load-bearing for §1.3's rejection of option (a), though not
  solely so.
- **API revision 89 is a moving pin.** Thirteen revisions ship for this part
  today. Each bump is a fork merge plus a layout-assertion run, and the plan
  assumes that stays mechanical. §1.7 is where that assumption gets tested.
- **The firmware is a black box we may not disassemble.** When it stops
  responding, the evidence available is a host-command transcript and whatever
  the firmware chooses to report. That is a genuinely worse debugging position
  than any other device in this tree.
- **`mvm/`'s MLD files may be on the AX210 path.** If recent firmware negotiates
  multi-link on this part, §1.6's exclusion of 2,561 lines is wrong and W8 grows.
- **Nothing in this plan is measurable against CLAUDE.md's 2× bar.** TCG cannot
  run this device at all, so throughput, latency and CPU cost are answerable only
  on the T14, same session, A/B — and the thing to A/B against is the I219 that
  does not exist yet either.
- **Doom first, and WLAN's prerequisites are shared with task #88.** If HDA moves
  the IOMMU stages, WLAN benefits; if it stalls them, WLAN stalls with it. The
  coupling is real and neither track owns it.
