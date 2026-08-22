//! Where every interrupt lands, counted on the CPU that took it.
//!
//! **The instrument before the change.** Every message-signalled interrupt in
//! this kernel is addressed to physical destination 0 (`drivers::pci`'s
//! `MSG_ADDR`) and the one I/O APIC pin this kernel routes goes to the BSP's
//! APIC id, so every device interrupt lands on the boot CPU and is spread from
//! there by `irq_ring` plus the scheduler. That is a design with a ceiling —
//! one CPU's interrupt bandwidth is the machine's — and the ceiling is only
//! worth moving against a number. This is that number: a per-CPU, per-source
//! delivery count, printed as one `irq: cpuN …` line per CPU beside the
//! process-exit census (`process::exit`), on `SYS_SHUTDOWN` and on the
//! blocked-task dump.
//!
//! **The counters are in `PerCpu`, so recording one is a single instruction.**
//! `irq_took!` emits exactly `add qword ptr gs:[<off>], 1` twice — once for the
//! machine's total and once for the source — with no register, no lock prefix
//! and no memory operand outside this CPU's own block. Two reasons that is
//! sound without a `lock`:
//!
//!   * **Same-CPU only.** Every writer is an interrupt handler indexing through
//!     `gs:`, which cannot name another CPU's block, and every reader is an
//!     `AtomicU64` load. There is no cross-CPU read-modify-write to lose.
//!   * **One instruction retires whole.** Interrupts — the NMI included — are
//!     taken at instruction boundaries, so an interrupt landing "inside" a
//!     non-locked `add [mem], 1` is not a state the machine has. This is the
//!     same argument `PerCpu::ring0_timer_fires`' plain `inc` rests on, and the
//!     opposite of `preempt_count`'s, which is a Rust load-add-store that an
//!     interrupt really can split.
//!
//! **The total is counted apart from the sources, and that is the whole of what
//! makes the census checkable.** `total` is not derived from the per-source
//! words: it is its own increment, written beside them, so a source whose
//! increment is missing shows up as `total > Σ sources` rather than as a number
//! that is quietly too small. `irq_census_conservation` is the gate that asks.
//!
//! **What it said the first time it was read.** Dev host, TCG, `cargo test
//! --test toyos-build` 12-wide, 2026-08-22: 43 of the run's 80 guests printed a
//! census — a test that boots and runs no program reaches no process exit — and
//! of **18,314** interrupts **12,239 (66.8%) were cpu0's**, per guest a median
//! of 87.9%. **Every one of the 7,962 device interrupts (xhci, net, sound,
//! i8042) was cpu0's**, on the dev host and on all twelve hosted KVM shards
//! alike; what is already spread is the timer and the shootdown IPI, which are
//! per-CPU by construction. An interrupt count is a function of timing and moves
//! between runs — the distribution is what to compare.
//! `issues/kernel/every-interrupt-lands-on-the-boot-cpu.md` carries both
//! machines' tables.
//!
//! **What is not counted, and why.** CPU exceptions are not interrupts a
//! placement policy will ever place — they are raised by the instruction stream
//! on the CPU that ran it — so `common_entry` counts nothing and the identity
//! above is unaffected. Vector 0xFD (the halt IPI) never returns: it is `cli;
//! hlt` forever, and a census the machine can no longer print is not one.
//! `LOG_NEST_VECTOR` is the `boot-actuators` self-IPI the log's own gate sends
//! itself; counting it would make the census's shape depend on a build feature,
//! which two builds cannot be compared across.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::percpu;
use crate::scheduler::MAX_CPUS;

/// Every interrupt source this kernel can be delivered and return from.
///
/// Exhaustive, and each variant is one IDT `direct` vector — `arch::idt`'s
/// table is what this list mirrors. Adding a device vector means adding a
/// variant here and one `irq_took!` at its handler.
///
/// **A variant nothing counts does not compile.** `irq_took!` is the only thing
/// that names one, so deleting an increment makes its variant unconstructed and
/// `-D dead-code` refuses the kernel — measured, by deleting `irq_took!(Xhci)`.
/// That covers the increment being *removed*; what it cannot cover is an
/// increment that runs on some deliveries and not others, which is what
/// `irq_census_conservation` is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Source {
    /// Vector 0x20: this CPU's own LAPIC one-shot, and every `apic::kick_cpu`
    /// IPI — they share a vector, so they share a counter.
    Timer,
    /// Vector 0x21, xHCI MSI-X (or MSI).
    Xhci,
    /// Vector 0x22, virtio-net MSI-X.
    Net,
    /// Vector 0x23, virtio-sound MSI-X.
    Sound,
    /// Vector 0x24, the i8042's I/O APIC pin — both PS/2 lines.
    I8042,
    /// Vector 0x25, the remapping unit's fault event.
    DmaFault,
    /// Vector 0x26, HDA stream completion.
    Hda,
    /// Vector 0xFE, the TLB shootdown IPI.
    Tlb,
    /// Vector 0x02, and `sched::dump` is its only sender.
    Nmi,
}

impl Source {
    pub const COUNT: usize = 9;

    /// The census's field names, in variant order. Read by the host side, so
    /// they are part of what a capture means: `tests/toyos.rs`'s
    /// `irq_census_conservation` parses them back.
    pub const NAMES: [&'static str; Self::COUNT] =
        ["timer", "xhci", "net", "sound", "i8042", "dmafault", "hda", "tlb", "nmi"];
}

/// How many `u64`s one CPU's counter block holds: the total, then one per
/// source. The layout is `PerCpu`'s, and `percpu::OFF_IRQ_COUNTS` is where it
/// starts.
pub const SLOTS: usize = 1 + Source::COUNT;

/// Index of the machine's own total inside a CPU's block.
pub const TOTAL: usize = 0;

/// The `gs:` displacement of slot `index` in this CPU's block.
pub const fn slot_offset(index: usize) -> u32 {
    percpu::OFF_IRQ_COUNTS + (index as u32) * 8
}

/// One delivery of `$source`, recorded on the CPU that took it.
///
/// Two instructions and nothing else — see the module header for why neither
/// needs a `lock` prefix. A macro rather than a function because the two
/// displacements have to be immediates: a `const` generic would put the
/// soundness of "one `add`" in the optimiser's hands, and this instrument's
/// entire claim is that its cost is that `add`.
macro_rules! irq_took {
    ($source:ident) => {{
        // SAFETY: both operands are `GS_BASE + <a slot of this CPU's own
        // counter block>`, whose offsets `arch::percpu` declares and asserts
        // against the type. Every caller is an interrupt handler, so `GS_BASE`
        // already points at this CPU's `PerCpu` — the same contract every
        // `gs:` access in this kernel has. `nostack` because neither
        // instruction touches the stack; no `nomem`, because both write; no
        // `preserves_flags`, because `add` writes six of them.
        unsafe {
            ::core::arch::asm!(
                "add qword ptr gs:[{total}], 1",
                "add qword ptr gs:[{source}], 1",
                total = const $crate::irq_census::slot_offset($crate::irq_census::TOTAL),
                source = const $crate::irq_census::slot_offset(
                    1 + $crate::irq_census::Source::$source as usize
                ),
                options(nostack),
            );
        }
    }};
}

pub(crate) use irq_took;

/// Every CPU's counter block, published as its `PerCpu` is built.
///
/// A registry rather than a walk of `PerCpu`s, because there is no other one:
/// a `PerCpu` is reached through `gs:` and `gs:` cannot name a sibling. What is
/// published is the address of the counter array alone, so a reader never forms
/// a reference covering the rest of the block — which the CPU that owns it
/// writes through raw pointers while this is being read.
static BLOCKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Publish one CPU's counter block. Called from `percpu::alloc_percpu`, which
/// runs on the BSP before the CPU it belongs to has executed an instruction.
pub(crate) fn publish(cpu_id: u32, block: *const AtomicU64) {
    let Some(slot) = BLOCKS.get(cpu_id as usize) else {
        return;
    };
    slot.store(block as u64, Ordering::Release);
}

/// One CPU's counters, or `None` if that CPU has never been built.
fn read(cpu: u32) -> Option<[u64; SLOTS]> {
    let base = BLOCKS.get(cpu as usize)?.load(Ordering::Acquire) as *const AtomicU64;
    if base.is_null() {
        return None;
    }
    let mut out = [0u64; SLOTS];
    for (i, slot) in out.iter_mut().enumerate() {
        // SAFETY: `base` is the `[AtomicU64; SLOTS]` inside a live `PerCpu`
        // that `publish` recorded and nothing ever frees, and `i < SLOTS`. The
        // `&AtomicU64` this forms covers one counter word and nothing else, and
        // every writer of that word is the owning CPU's single `add` — so the
        // load is a plain atomic read of a word under interior mutability.
        *slot = unsafe { (*base.add(i)).load(Ordering::Relaxed) };
    }
    Some(out)
}

/// `name=value` for every source, in [`Source::NAMES`] order.
struct Fields<'a>(&'a [u64]);

impl fmt::Display for Fields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, count) in Source::NAMES.iter().zip(self.0) {
            write!(f, " {name}={count}")?;
        }
        Ok(())
    }
}

/// One `irq: cpuN total=… <source>=… …` line per online CPU.
///
/// The counts are cumulative since boot, so the last such line a capture holds
/// is that boot's whole census and the difference between two of them is what
/// the interval cost. Nothing here allocates, takes a lock or touches a device.
pub fn log_census() {
    for cpu in 0..crate::arch::smp::cpu_count() {
        let Some(counts) = read(cpu) else { continue };
        crate::log!(
            "irq: cpu{cpu} total={}{}",
            counts[TOTAL],
            Fields(&counts[TOTAL + 1..])
        );
    }
}
