//! Loom: the sleep handshake and the doorbell's kick accounting
//! (spec §7.3, §7.5, invariant I2).
//!
//! The handshake is: publish SLEEPING, drain, check the mailbox one last
//! time, then halt. A producer that posts after that check raises the KICK
//! edge with SLEEPING already visible, so it sends the targeted IPI and the
//! halt cannot be a sleep-through. The IPI is modelled as a counter, because
//! its only relevant effect is "the target cannot stay halted".
//!
//! Two producers, so the edge-coalescing rule of §7.3 (a normal wake to a
//! busy CPU costs zero IPIs; a sleeping CPU always gets one) is exercised
//! rather than assumed.
//!
//! The edge the property rests on is `ring`'s read of the doorbell's bits: it
//! is what lets a producer see a target's freshly published SLEEPING before
//! deciding to elide the IPI. On x86 every read-modify-write is a full fence,
//! so a build with it relaxed behaves identically to this one and no guest
//! test can fail here. The negative case is a cargo feature rather than a
//! comment:
//!
//! ```text
//! cargo test -p toyos-sched-loom --features doorbell-kick-relaxed \
//!   --test loom_sleep
//! ```
//!
//! makes that read relaxed and this file must red, at
//! [`a_halted_cpu_with_queued_work_was_kicked`] — *halted with 2 of 2 messages
//! queued and no IPI in flight — a sleep-through (spec §7.5)*, stated exactly.
//! Verified 2026-08-17, both ways round.

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;
use toyos_sched_loom::mailbox::{
    mailbox, Doorbell, Kick, MailboxConsumer, MailboxNode, MailboxProducer, Urgency,
};
use toyos_sched_loom::model::{model, Msg, PreemptModel, RemoteGuard};

struct World {
    tx: MailboxProducer<Msg>,
    doorbell: Doorbell,
    preempt: PreemptModel,
    nodes: [MailboxNode<Msg>; 2],
    ipis: AtomicUsize,
}

impl World {
    fn new(tx: MailboxProducer<Msg>) -> Self {
        Self {
            tx,
            doorbell: Doorbell::new(),
            preempt: PreemptModel::new(),
            nodes: [MailboxNode::new(), MailboxNode::new()],
            ipis: AtomicUsize::new(0),
        }
    }

    fn produce(&self, which: usize) {
        self.tx.post(
            self.nodes[which].claim().expect("one message per node"),
            Msg::Probe(which as u32),
            &RemoteGuard,
        );
        if self.doorbell.ring(Urgency::Normal) == Kick::Send {
            self.ipis.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// One scheduler pass, ending in the idle disposition. Returns whether
    /// the CPU would halt.
    fn pass(&self, rx: &mut MailboxConsumer<Msg>, drained: &mut Vec<Msg>) -> bool {
        let guard = self.preempt.disable();
        self.doorbell.begin_pass();
        while let Some(msg) = rx.pop(&guard) {
            drained.push(msg);
        }
        self.doorbell.arm_sleep().confirm(rx).is_ok()
    }
}

/// I2: if the CPU halts while work is still queued, an IPI must already be on
/// its way — otherwise nothing would ever wake it.
#[test]
fn a_halted_cpu_with_queued_work_was_kicked() {
    model(|| {
        let (tx, mut rx) = mailbox::<Msg>();
        let world = Arc::new(World::new(tx));

        let producers: Vec<_> = (0..2)
            .map(|which| {
                let world = world.clone();
                loom::thread::spawn(move || world.produce(which))
            })
            .collect();

        let mut drained = Vec::new();
        let halted = world.pass(&mut rx, &mut drained);

        for producer in producers {
            producer.join().unwrap();
        }

        if halted && drained.len() < 2 {
            assert!(
                world.ipis.load(Ordering::SeqCst) >= 1,
                "halted with {} of 2 messages queued and no IPI in flight — \
                 a sleep-through (spec §7.5)",
                2 - drained.len(),
            );
        }

        // The wake-up pass drains the rest; nothing is lost and every node is
        // free again (otherwise the nodes' drop bomb fires at teardown).
        world.pass(&mut rx, &mut drained);
        drained.sort_by_key(|m| match m {
            Msg::Probe(n) => *n,
            other => panic!("unexpected message {other:?}"),
        });
        assert_eq!(drained, [Msg::Probe(0), Msg::Probe(1)]);
        assert!(world.nodes.iter().all(|n| !n.in_flight()));
    });
}

/// The handshake's liveness in the model: a halted CPU is always brought back
/// by the edge or the IPI, and the pass it then runs drains everything. The
/// bound of three passes is the model's, not the protocol's — a fourth would
/// only re-check an already-empty queue.
#[test]
fn a_kicked_cpu_comes_back_and_drains_everything() {
    model(|| {
        let (tx, mut rx) = mailbox::<Msg>();
        let world = Arc::new(World::new(tx));

        let producers: Vec<_> = (0..2)
            .map(|which| {
                let world = world.clone();
                loom::thread::spawn(move || world.produce(which))
            })
            .collect();

        let mut drained = Vec::new();
        for _ in 0..3 {
            if world.pass(&mut rx, &mut drained) && drained.len() == 2 {
                break;
            }
        }
        for producer in producers {
            producer.join().unwrap();
        }
        world.pass(&mut rx, &mut drained);

        assert_eq!(drained.len(), 2, "every posted message is delivered");
        assert!(world.nodes.iter().all(|n| !n.in_flight()));
        assert!(
            world.ipis.load(Ordering::SeqCst) <= 2,
            "at most one IPI per post; coalesced edges send none",
        );
    });
}
