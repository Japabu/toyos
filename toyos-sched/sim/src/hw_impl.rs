//! `SimHw` — the simulator's side of the [`Hw`] boundary (spec §10.1).
//!
//! Real and shared with the kernel: task types, state word, transitions, run
//! queue, fairness math, mailbox, doorbell, sleep handshake, ticket protocol,
//! retire chase, deadline heap, pass logic. Mocked here: time, timer, IPI,
//! halt, switch. Nothing above the line is reimplemented, which is the whole
//! point — a simulator that models the scheduler instead of *being* it proves
//! nothing about the code that ships.

use std::collections::HashMap;
use std::sync::Mutex;

use toyos_sched::cpu::{RunToken, SleepToken};
use toyos_sched::hw::{CpuId, Hw, Kicker, Nanos, TraceEvent};
use toyos_sched::task::{TaskAccounting, TaskKey};

use crate::payload::{SimCtx, SimPayload};

/// A task's context has been saved since it last ran — the shadow invariant
/// I11 watches. `false` means the task is the one currently loaded on some
/// CPU, and moving or finalizing it would be the park-before-switch ordering
/// bug the kernel's `outgoing`/`handle_outgoing` machinery used to make
/// possible.
#[derive(Debug, Default)]
pub struct SimHwState {
    pub now: Nanos,
    pub armed: Vec<Option<Nanos>>,
    pub pending_ipi: Vec<u32>,
    pub need_resched: Vec<bool>,
    pub halted: Vec<bool>,
    pub ctx_saved: HashMap<TaskKey, bool>,
    pub loaded: Vec<Option<TaskKey>>,
    pub released: Vec<(TaskKey, TaskAccounting)>,
    pub trace: Vec<TraceEvent>,
    pub kicks: u64,
    pub switches: u64,
    /// Which CPU's pass is running. The core has no `cpu_id()` by design, so
    /// the driver is what knows where a timer or a halt lands.
    pub current: Option<CpuId>,
    /// Recorded rather than raised: a step machine has no stack to unwind
    /// from inside a `Hw` callback, and swallowing the first violation would
    /// hide it behind the cascade it causes.
    pub violations: Vec<String>,
}

pub struct SimHw {
    state: Mutex<SimHwState>,
}

impl SimHw {
    pub fn new(cpus: usize) -> Self {
        Self {
            state: Mutex::new(SimHwState {
                now: Nanos::ZERO,
                armed: vec![None; cpus],
                pending_ipi: vec![0; cpus],
                need_resched: vec![false; cpus],
                halted: vec![false; cpus],
                ctx_saved: HashMap::new(),
                loaded: vec![None; cpus],
                released: Vec::new(),
                trace: Vec::new(),
                kicks: 0,
                switches: 0,
                current: None,
                violations: Vec::new(),
            }),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut SimHwState) -> R) -> R {
        f(&mut self.state.lock().expect("single-threaded"))
    }

    /// The timer a pass programs is *for the CPU running that pass*, and the
    /// core deliberately has no `cpu_id()` to tell us which that is (spec
    /// §10.1). The VM sets this before every pass, so `set_timer` knows where
    /// the effect lands without the core ever asking an ambient question.
    pub fn enter_pass(&self, cpu: CpuId, now: Nanos) {
        self.with(|s| {
            s.now = now;
            s.current = Some(cpu);
        });
    }

    pub fn leave_pass(&self) {
        self.with(|s| s.current = None);
    }

    fn cpu(state: &SimHwState) -> usize {
        state
            .current
            .expect("a Hw effect outside a pass has no CPU to land on")
            .0 as usize
    }

    fn violate(state: &mut SimHwState, what: String) {
        state.violations.push(what);
    }
}

impl Kicker for SimHw {
    fn kick(&self, target: CpuId) {
        self.with(|s| {
            s.pending_ipi[target.0 as usize] += 1;
            s.kicks += 1;
        });
    }
}

impl Hw for SimHw {
    type Payload = SimPayload;
    /// A step is atomic in this world, so "IRQs off" is the default rather
    /// than a state to enter: delivery steps are simply not enabled while a
    /// pass runs. The guard is therefore a witness with nothing to carry.
    type IrqGuard = ();

    fn now(&self) -> Nanos {
        self.with(|s| s.now)
    }

    fn set_timer(&self, deadline: Nanos) {
        self.with(|s| {
            let cpu = Self::cpu(s);
            s.armed[cpu] = Some(deadline);
        });
    }

    fn stop_timer(&self) {
        self.with(|s| {
            let cpu = Self::cpu(s);
            s.armed[cpu] = None;
        });
    }

    fn irq_guard(&self) {}

    #[allow(unsafe_code)] // the trait method is `unsafe fn`; this body derefs nothing
    unsafe fn switch(&self, token: RunToken<SimPayload>) {
        self.with(|s| {
            let cpu = Self::cpu(s);
            s.switches += 1;
            if let Some(outgoing) = token.outgoing() {
                // The context is saved by the switch itself — which is
                // exactly why park-before-switch is safe only when nothing
                // touches the task between the park and here.
                s.ctx_saved.insert(outgoing, true);
            }
            if let Some(incoming) = token.incoming() {
                if !s.ctx_saved.get(&incoming).copied().unwrap_or(true) {
                    Self::violate(
                        s,
                        format!("I11: resumed {incoming:?} whose context was never saved"),
                    );
                }
                s.ctx_saved.insert(incoming, false);
            }
            s.loaded[cpu] = token.incoming();
        });
    }

    fn idle_wait(&self, _token: SleepToken) {
        self.with(|s| {
            let cpu = Self::cpu(s);
            s.halted[cpu] = true;
        });
    }

    fn need_resched(&self, cpu: CpuId) {
        self.with(|s| s.need_resched[cpu.0 as usize] = true);
    }

    fn release(&self, key: TaskKey, payload: SimPayload, acct: TaskAccounting) {
        self.with(|s| {
            if !s.ctx_saved.get(&key).copied().unwrap_or(true) {
                Self::violate(
                    s,
                    format!("I11: finalized {key:?} whose context was never saved"),
                );
            }
            s.ctx_saved.remove(&key);
            s.released.push((key, acct));
        });
        // Dropping the payload here releases its address-space Arc. Invariant
        // I8 re-counts the refs after every step; this is the drop it counts.
        drop(payload);
    }

    fn trace(&self, ev: TraceEvent) {
        self.with(|s| s.trace.push(ev));
    }
}

/// A context slot the VM hands to `CpuSched::new` for its idle context.
pub fn idle_ctx() -> SimCtx {
    SimCtx::default()
}
