//! Loom: the publish/observe edge `Balance::PushOnSurplus` is made of.
//!
//! The pull half of the balance path needs no ordering between two CPUs at all:
//! an idle CPU reads a surplus, and if it reads a stale one it simply does not
//! probe. The push half is different in kind, because it is a *liveness* claim
//! that rests on one CPU observing another — the CPU that gained surplus reads
//! the SLEEPING bit of the CPU that is going to sleep, and decides from it
//! whether anybody needs telling.
//!
//! That makes it the store-buffer litmus test:
//!
//! ```text
//! pusher                             sleeper
//! ------                             -------
//! surplus.store(2)                   doorbell.fetch_or(SLEEPING)
//! balance_fence()                    balance_fence()
//! doorbell.load()   -> asleep?       surplus.load()   -> anything to steal?
//! ```
//!
//! Each side stores to one location and loads the other, and with anything
//! weaker than a `SeqCst` fence between the two, **both loads may miss**: the
//! pusher decides nobody is asleep, the sleeper decides there is nothing to come
//! for, and the machine is back in
//! `issues/kernel/an-idle-cpu-that-slept-before-the-surplus-is-never-probed.md`
//! in a window nanoseconds wide instead of milliseconds. Nothing in a guest test
//! can find that — on x86 the sleeper's side of it is a `lock or`, a full fence
//! already, so only the pusher's half is even reorderable and only under a
//! store-buffer delay no test can schedule.
//!
//! The property below is the one the push actually needs: **a CPU that halts
//! without having seen the surplus has a poke coming.** It is the same shape as
//! `loom_sleep.rs`'s I2 — halted with work queued implies an IPI in flight —
//! asked of the push's own pair rather than of the mailbox's.
//!
//! The negative control is a cargo feature rather than a comment:
//!
//! ```text
//! cargo test -p toyos-sched-loom --features push-fence-relaxed --test loom_push
//! ```
//!
//! makes `cpu::balance_fence` a release fence — which orders stores against
//! stores and nothing against a later load — and this file must red at
//! [`a_cpu_that_halts_without_seeing_the_surplus_was_pushed`]. Verified
//! 2026-08-22, both ways round.

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;
use toyos_sched_loom::cpu::{balance_fence, CpuHandle};
use toyos_sched_loom::hw::CpuId;
use toyos_sched_loom::mailbox::{mailbox, Kick, MailboxConsumer};
use toyos_sched_loom::model::{model, Msg};

/// The surplus at which a pass pushes and at which an idle pass probes — one
/// inequality, `SchedPass::best_victim`'s.
const THRESHOLD: u32 = 2;

/// Two CPUs' shared faces: the one that gains surplus, and the one going to
/// sleep. The `CpuHandle` is the real one, so `publish_surplus`, `surplus`,
/// `doorbell` and `poke` are the shipped code and not a restatement of it.
struct World {
    /// The CPU that gains surplus. Only its `surplus` matters here.
    victim: CpuHandle<Msg>,
    /// The CPU going to sleep. Only its doorbell matters here.
    thief: CpuHandle<Msg>,
    /// Targeted IPIs the push sent. As in `loom_sleep.rs`, an IPI's only
    /// relevant effect is "the target cannot stay halted".
    ipis: AtomicUsize,
    /// Doorbell rings the push made, kicked or coalesced. A ring that elides its
    /// IPI is still an observation — it can only elide because an IPI is already
    /// on its way — so the property counts rings and not kicks.
    pokes: AtomicUsize,
}

impl World {
    /// `SchedPass::push_on_surplus`, as the pass runs it: publish, fence, look
    /// for a sleeper, ring it.
    fn publish_and_push(&self) {
        self.victim.publish_surplus(THRESHOLD);
        balance_fence();
        if self.thief.doorbell().sleeping() {
            self.pokes.fetch_add(1, Ordering::AcqRel);
            if self.thief.poke() == Kick::Send {
                self.ipis.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    /// `SchedPass::try_sleep` under `Balance::PushOnSurplus`, with the part that
    /// matters: the probe's own read of the surplus found nothing, SLEEPING is
    /// published, and the re-read behind the fence is the load that pairs with
    /// the pusher's read of that bit.
    ///
    /// Returns `true` if the CPU halted **without** having seen the surplus —
    /// the state the push has to make impossible without a poke behind it.
    fn try_sleep(&self, rx: &MailboxConsumer<Msg>) -> bool {
        self.thief.doorbell().begin_pass();
        // `post_steal_probe`'s read, before SLEEPING is published. A stale
        // answer here is exactly what the re-read below exists to catch.
        let probed = self.victim.surplus() >= THRESHOLD;
        let arm = self.thief.doorbell().arm_sleep();
        balance_fence();
        if self.victim.surplus() >= THRESHOLD {
            // Observed: the pass goes round again and posts a probe.
            arm.abandon();
            return false;
        }
        let halted = arm.confirm(rx).is_ok();
        halted && !probed
    }
}

/// The push's liveness: a CPU cannot halt believing there is nothing to steal
/// while another CPU has already published a surplus and believes nobody is
/// asleep.
///
/// One of the two has to observe the other, and the `SeqCst` fence in
/// [`balance_fence`] is the whole of why. The assertion is stated on the poke
/// and not on the IPI for the reason `Doorbell::ring` gives: a ring that returns
/// `Kick::Elide` did so because the kick edge was already raised, i.e. because a
/// kick is already in flight.
#[test]
fn a_cpu_that_halts_without_seeing_the_surplus_was_pushed() {
    model(|| {
        let (victim_tx, _victim_rx) = mailbox::<Msg>();
        let (thief_tx, thief_rx) = mailbox::<Msg>();
        let world = Arc::new(World {
            victim: CpuHandle::new(CpuId(0), victim_tx),
            thief: CpuHandle::new(CpuId(1), thief_tx),
            ipis: AtomicUsize::new(0),
            pokes: AtomicUsize::new(0),
        });

        let pusher = {
            let world = world.clone();
            loom::thread::spawn(move || world.publish_and_push())
        };
        let blind = world.try_sleep(&thief_rx);
        pusher.join().unwrap();

        assert!(
            !blind || world.pokes.load(Ordering::SeqCst) >= 1,
            "cpu1 halted with cpu0's surplus of {THRESHOLD} published and no push behind it — \
             both sides of the store-buffer pair missed, which is \
             `an-idle-cpu-that-slept-before-the-surplus-is-never-probed` in a window a fence \
             would have closed",
        );
    });
}

/// The other direction, and the one that says the fence is not simply making
/// everything look asleep: the push does not ring a CPU that is awake.
///
/// A ring on a busy target sets the kick edge with no message behind it, which
/// costs that target's next idle disposition a spurious extra pass — harmless,
/// and exactly what the push is allowed to cost. What it may not do is ring a
/// CPU that never armed a sleep at all, because then the count this policy is
/// priced by is measuring something that is not a wake.
#[test]
fn a_push_rings_nobody_when_nobody_is_asleep() {
    model(|| {
        let (victim_tx, _victim_rx) = mailbox::<Msg>();
        let (thief_tx, _thief_rx) = mailbox::<Msg>();
        let world = Arc::new(World {
            victim: CpuHandle::new(CpuId(0), victim_tx),
            thief: CpuHandle::new(CpuId(1), thief_tx),
            ipis: AtomicUsize::new(0),
            pokes: AtomicUsize::new(0),
        });

        // The target runs a pass and stays awake: `begin_pass` clears SLEEPING
        // and nothing arms one.
        let target = {
            let world = world.clone();
            loom::thread::spawn(move || world.thief.doorbell().begin_pass())
        };
        world.publish_and_push();
        target.join().unwrap();

        assert_eq!(
            world.pokes.load(Ordering::SeqCst),
            0,
            "the push rang a CPU that never published SLEEPING — a wake charged to a policy \
             that did not need to send it",
        );
        assert_eq!(world.ipis.load(Ordering::SeqCst), 0, "and no IPI either");
    });
}
