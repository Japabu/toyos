//! Loom: the intrusive MPSC under torn pushes (spec §7.2, §12).
//!
//! One model, two configurations. The threads are the three producers a CPU
//! actually has — its own thread context, its own IRQ context, and another
//! CPU — plus the scheduler pass that IRQ exit may run:
//!
//! * `local`  — CPU0 thread context, pushing to CPU0's own mailbox under a
//!              [`PreemptModel`] guard (the mandatory preempt-disabled push).
//! * `irq`    — CPU0 IRQ context: pushes (interrupting `local`'s push in some
//!              interleavings), then at IRQ exit runs a pass *if* preemption
//!              is enabled.
//! * `remote` — another CPU: pushes and rings with `Urgency::Normal`, so a
//!              busy target gets no IPI (spec §7.3) — its message is the one
//!              that ends up behind the unlinked suffix.
//!
//! Two properties are checked.
//!
//! **Delivery (both configurations).** No message is ever lost: a torn push
//! delays the suffix by the pusher's remaining instructions and the following
//! doorbell edge brings the consumer back. The terminal assertion after every
//! producer has joined is that one pass drains all three messages and every
//! embedded node is free again.
//!
//! **I2 (the reason the guard is mandatory).** A CPU must not halt while its
//! own interrupted context is inside a push. That context cannot resume while
//! its CPU is halted, so the suffix behind its unlinked node — every message
//! other CPUs pushed after it, whose doorbell edges this pass just consumed —
//! is stranded with nothing left to raise an edge. Preempt-disable is exactly
//! what makes the state unreachable: the IRQ-exit pass and a preempt-disabled
//! push exclude each other, so the pass never observes a half-linked local
//! push. `PreemptModel::disable` models that exclusion, and
//! `--features no-preempt-guard` models it away — at which point loom finds
//! the forbidden interleaving, which is what
//! `preempted_producer_strands_suffix` asserts.

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc;
use toyos_sched_loom::mailbox::{
    mailbox, Doorbell, MailboxConsumer, MailboxNode, MailboxProducer, Urgency,
};
use toyos_sched_loom::model::{model, IrqGuard, Msg, PreemptModel, RemoteGuard};

const LOCAL: Msg = Msg::Probe(1);
const IRQ: Msg = Msg::Probe(2);
const REMOTE: Msg = Msg::Probe(3);

struct World {
    tx: MailboxProducer<Msg>,
    doorbell: Doorbell,
    preempt: PreemptModel,
    local_node: MailboxNode<Msg>,
    irq_node: MailboxNode<Msg>,
    remote_node: MailboxNode<Msg>,
    /// CPU0's own thread context is inside `post` right now — i.e. this CPU
    /// has an interrupted context that owes the queue a link store.
    local_pushing: AtomicBool,
}

fn pass(world: &World, rx: &mut MailboxConsumer<Msg>, drained: &mut Vec<Msg>) {
    let Some(guard) = world.preempt.enter_pass() else {
        // Preemption is disabled in the interrupted context: IRQ exit returns
        // to it instead of running a pass (spec §6.3).
        return;
    };
    // Clear the edge before draining, so a message posted after the drain
    // re-raises it (spec §7.3).
    world.doorbell.begin_pass();
    while let Some(msg) = rx.pop(&guard) {
        drained.push(msg);
    }
    // Nothing else is runnable in this model, so the pass reaches the idle
    // disposition and tries to halt.
    if world.doorbell.arm_sleep().confirm(rx).is_ok() {
        assert!(
            !world.local_pushing.load(Ordering::SeqCst),
            "I2: this CPU halted while its own interrupted context is inside a \
             push. That context cannot resume while the CPU sleeps, so every \
             message queued behind its unlinked node stays invisible with no \
             doorbell edge left to raise (spec §7.2). Drained so far: \
             {drained:?}",
        );
    }
}

fn mailbox_model() {
    let (tx, mut rx) = mailbox::<Msg>();
    let world = Arc::new(World {
        tx,
        doorbell: Doorbell::new(),
        preempt: PreemptModel::new(),
        local_node: MailboxNode::new(),
        irq_node: MailboxNode::new(),
        remote_node: MailboxNode::new(),
        local_pushing: AtomicBool::new(false),
    });

    let local = {
        let world = world.clone();
        loom::thread::spawn(move || {
            let guard = world.preempt.disable();
            world.local_pushing.store(true, Ordering::SeqCst);
            world
                .tx
                .post(world.local_node.claim().unwrap(), LOCAL, &guard);
            world.local_pushing.store(false, Ordering::SeqCst);
            drop(guard);
            let _ = world.doorbell.ring(Urgency::Normal);
        })
    };

    let remote = {
        let world = world.clone();
        loom::thread::spawn(move || {
            world
                .tx
                .post(world.remote_node.claim().unwrap(), REMOTE, &RemoteGuard);
            let _ = world.doorbell.ring(Urgency::Normal);
        })
    };

    let irq = {
        let world = world.clone();
        loom::thread::spawn(move || {
            let mut drained = Vec::new();
            world.tx.post(world.irq_node.claim().unwrap(), IRQ, &IrqGuard);
            let _ = world.doorbell.ring(Urgency::Normal);
            pass(&world, &mut rx, &mut drained);
            (rx, drained)
        })
    };

    local.join().unwrap();
    remote.join().unwrap();
    let (mut rx, mut drained) = irq.join().unwrap();

    // Delivery: the next pass after every producer has finished finds
    // everything a torn push delayed.
    pass(&world, &mut rx, &mut drained);
    drained.sort_by_key(|m| match m {
        Msg::Probe(n) => *n,
        other => panic!("unexpected message {other:?}"),
    });
    assert_eq!(drained, [LOCAL, IRQ, REMOTE], "no message may be lost");
    // Every node is free again — otherwise the drop bomb in `World`'s nodes
    // would fire when the model tears down.
    assert!(!world.local_node.in_flight());
    assert!(!world.irq_node.in_flight());
    assert!(!world.remote_node.in_flight());
}

/// The positive case: with the mandatory preempt-disabled push, no schedule
/// reaches the forbidden state, and nothing is ever lost.
#[cfg(not(feature = "no-preempt-guard"))]
#[test]
fn preempt_disabled_push_keeps_the_queue_drainable() {
    model(mailbox_model);
}

/// The mandated negative case: with the guard modelled away the very same
/// model MUST fail. Run it with
/// `cargo test -p toyos-sched-loom --features no-preempt-guard`; the caught
/// panic (printed by loom together with the offending execution) is the
/// evidence.
#[cfg(feature = "no-preempt-guard")]
#[test]
fn preempted_producer_strands_suffix() {
    // `TOYOS_LOOM_RAW=1` skips the catch, so the case is reported as an
    // ordinary test failure with loom's offending execution printed — the
    // demonstration that "must fail" really does fail.
    if std::env::var_os("TOYOS_LOOM_RAW").is_some() {
        model(mailbox_model);
        panic!("the model ran clean: the stranded suffix was NOT detected");
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| model(mailbox_model)));
    assert!(
        result.is_err(),
        "modelling the preempt guard away must expose the stranded suffix",
    );
}

/// §7.7: the steal probe is one reusable node. A thief may only post when its
/// previous probe has been consumed, and the victim releases the node
/// strictly after unlinking it — so the node is never linked twice.
#[cfg(not(feature = "no-preempt-guard"))]
#[test]
fn steal_probe_node_is_never_double_linked() {
    model(|| {
        let (tx, mut rx) = mailbox::<Msg>();
        let world = Arc::new((tx, MailboxNode::<Msg>::new(), PreemptModel::new()));

        let thief = {
            let world = world.clone();
            loom::thread::spawn(move || {
                let mut posted = 0;
                for probe in 0..2 {
                    let guard = world.2.disable();
                    // No probe is posted while one is outstanding; the thief
                    // simply doesn't post another (spec §7.7).
                    if let Some(slot) = world.1.claim() {
                        world.0.post(slot, Msg::Probe(probe), &guard);
                        posted += 1;
                    }
                }
                posted
            })
        };

        let victim = {
            let world = world.clone();
            loom::thread::spawn(move || {
                let guard = world.2.disable();
                let mut got = 0;
                while rx.pop(&guard).is_some() {
                    got += 1;
                }
                (rx, got)
            })
        };

        let posted = thief.join().unwrap();
        let (mut rx, mut got) = victim.join().unwrap();
        let guard = world.2.disable();
        while rx.pop(&guard).is_some() {
            got += 1;
        }
        assert_eq!(got, posted, "every posted probe is consumed exactly once");
        assert!(!world.1.in_flight(), "the node is free after consumption");
    });
}
