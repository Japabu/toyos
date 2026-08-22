//! The conservation law, read through `SYS_LOG_READ` from inside `test-runner`.
//!
//! **It runs here rather than in a binary of its own, and that is capability
//! doctrine rather than convenience.** `test-runner` passes its whole
//! *namespace* to every binary it spawns, and `logread` is not a namespace
//! entry — it is a `SysCap` dup, exactly like `realtime`, which the estate does
//! not hand down either. So the gate that reads the machine's log is the one
//! process in a test image that holds the right from its own manifest row.
//!
//! **The verdict is exact, not statistical.** Every sequence number a shard
//! ever issued is either a record this reader took or one the kernel counted as
//! lost; no number is taken twice; and every storm record's text regenerates
//! byte for byte from the two numbers it declares. A torn record fails the
//! text, a lost record that is not counted fails the ledger, and a duplicated
//! one fails it the other way.
//!
//! **Nothing this reader waits for is a record the ring may drop.** It used to
//! read until every producer had said `logstorm done`, and that record is the
//! last thing one producer writes rather than the last thing written to its
//! shard: two producers placed on one CPU means the second's records lap the
//! first's `done`, and the loop then waited for something that was never
//! coming — twice in seven suites on the dev host, each time the whole 30 s
//! ceiling in the fast tier. So the termination condition is the *cursor*: the
//! log has been drained and nothing new has arrived for [`QUIET_READS`] reads
//! and [`STORM_SETTLE`] of guest time. A `done` is a cross-check where it
//! survived and is never waited on, and the same holds of `logstorm start` and
//! of the nesting burst's own `done`. **The rule this shape exists to keep is
//! general**: a workload whose liveness depends on a record the ring is allowed
//! to drop is the same mistake wherever it appears.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use toyos::log::{LogTail, Record, MAX_LOG_SHARDS};
use toyos::poller::{Poller, READABLE};
use toyos::syscap::SysCap;

/// The first sequence number any shard issues — one, so a slot nothing has ever
/// written cannot read as record 0 of every shard on every boot.
/// `kernel/src/log/shard.rs`'s `FIRST_SEQ` is the other half of this constant.
const FIRST_SEQ: u64 = 1;

/// Records per `SYS_LOG_READ`. Above the shard count, which the call refuses
/// below, and far under a storm's rate — so the reader really is outrun and the
/// loss path is reached rather than assumed.
const BATCH: usize = 64;

/// How long the whole gate may take before it gives up on a workload that never
/// finished, and it reports what it had when it did.
///
/// **A liveness guard and never a verdict**, and it is what a shard stalled on
/// an uncommitted slot looks like from here: `drain_ordered` blocks a shard at
/// its first uncommitted record, so a writer that never publishes takes that
/// shard out of the merge for good. A green run is under a second of guest
/// time at every width this gate is booted at.
///
/// **It is the guest's own ceiling and it is the smaller of the two**: the host
/// gives the whole boot 60 s (`tests/common/logread.rs`), so what a hung gate
/// reports is this one's message and this one's elapsed time. Nothing in the
/// loop below waits on a record any more, so reaching it now means the kernel
/// stopped answering rather than that a record went missing.
const CEILING: Duration = Duration::from_secs(30);

/// Empty reads in a row before the log is called quiet.
///
/// **Eight, each after a bounded park on the readiness source**, because a
/// single empty read can land while a producer is inside its publication
/// bracket: `drain_ordered` stops that shard and says nothing about it, so a
/// ledger closed on the first empty read can be short by what was in flight.
///
/// **It is the whole termination condition now**, so what it costs when it is
/// wrong is worth stating: a quiet run that lands mid-storm ends the read early
/// and the verdict is computed over less of the workload. It cannot make the
/// verdict *wrong* — the conservation law is over the sequence numbers this
/// reader took and the loss the kernel counted for the same cursor, and both
/// are a consistent snapshot at any point — and the non-vacuity clauses in
/// [`verdict`] are what refuse a run that raced nothing. [`STORM_SETTLE`] is
/// what makes an early end implausible rather than merely unlikely.
const QUIET_READS: u32 = 8;

/// How long after the last producer record the log must stay quiet before a
/// storm counts as finished.
///
/// Eight empty reads are sixteen milliseconds of parks, and a producer stalled
/// inside its publication bracket for that long — a vCPU that the host has not
/// scheduled, which is the twelve-wide suite's ordinary state — takes its shard
/// out of the merge and can leave every other shard drained. A hundred
/// milliseconds of *guest* time on top costs one tenth of a second on three
/// boots and buys an order of magnitude on that window. It is armed only once a
/// producer's record has been seen, so an ordinary boot's gate ends on the
/// quiet reads alone.
const STORM_SETTLE: Duration = Duration::from_millis(100);

/// How long a park on the log's readiness source waits before giving up on it.
///
/// It is the gate's pacing as much as its wait: with nothing left to say the
/// kernel posts nothing, and eight of these is the whole tail of the run.
const IDLE_NANOS: u64 = 2_000_000;

/// How long the deterministic readiness round waits for its own record.
///
/// Generous, because what it bounds is a scheduler getting round to a child's
/// exit on a machine that has just run a storm on every CPU — not the post,
/// which is one function call after the drain. A gate that timed out here would
/// be reporting the host's load and not the kernel's.
const READINESS_WAIT_NANOS: u64 = 2_000_000_000;

/// The poll's token. One handle is watched, so it identifies the round rather
/// than the source.
const LOG_TOKEN: u64 = 1;

/// `kernel/src/log/storm.rs`'s `PAYLOAD`.
const PAYLOAD: usize = 96;

/// `kernel/src/log/nested.rs`'s `NEST_PRODUCER`: the burst an interrupt handler
/// emits declares itself as this, so it goes through the same per-producer
/// ledger and the same byte-for-byte regeneration as a storm's records.
const NEST_PRODUCER: u64 = u64::MAX;


/// One storm producer's ledger.
#[derive(Default)]
struct Producer {
    /// The next index expected from this thread, and `None` before its first
    /// record.
    next: Option<u64>,
    read: u64,
    /// What its own `done` record declared, once seen.
    emitted: Option<u64>,
    /// Shards this producer's records were found on. **More than one is a
    /// producer that migrated mid-storm**, and on this kernel that is zero of
    /// them and always will be: nothing switches a Ring 0 context out between
    /// two instructions, so a producer cannot be moved off its CPU inside the
    /// reservation window
    /// (`kernel/src/log/storm.rs`'s header carries the measurement). It is
    /// reported and asserted on by nothing, which is the honest shape for a
    /// count whose only interesting value is unreachable.
    shards: u32,
    shard_mask: u32,
}

impl Producer {
    fn mark_shard(&mut self, cpu: u16) {
        let bit = 1u32 << (cpu as u32 % 32);
        if self.shard_mask & bit == 0 {
            self.shard_mask |= bit;
            self.shards += 1;
        }
    }
}

/// One shard's ledger: the sequence numbers the kernel issued on that CPU.
#[derive(Default, Clone, Copy)]
struct ShardLedger {
    first: Option<u64>,
    next: u64,
    read: u64,
    /// Sequence numbers this reader never saw, derived from the gaps between
    /// the ones it did.
    gaps: u64,
    last_at_ns: u64,
}

pub fn run(cap: Option<&SysCap>) -> i32 {
    let Some(cap) = cap else {
        println!("log-gate: this program holds no system capability, so it holds no `logread`");
        return 1;
    };
    match gate(cap) {
        Ok(()) => 0,
        Err(e) => {
            println!("log-gate: FAILED: {e}");
            1
        }
    }
}

struct Run {
    shards: [ShardLedger; MAX_LOG_SHARDS],
    producers: BTreeMap<u64, Producer>,
    /// Producers this machine's storm has, once one of its records has been
    /// seen. **Derived from the shard count rather than from an announcement**:
    /// the storm starts inside the reader's own first `SYS_LOG_READ` and can
    /// lap a shard before that call returns, so its opening line is a record
    /// like any other and may be dropped. One thread per shard is what
    /// `log::storm::start_once` spawns, and the cursor is what says how many
    /// shards there are.
    storm: Option<u32>,
    /// What `logstorm start` or a producer's `done` declared, where one of
    /// those records survived. They must agree. **A cross-check and never a
    /// requirement**: both kinds are records like any other and the ring is
    /// allowed to drop either, so [`verdict`] derives the count from the
    /// highest index any producer reached when neither arrives.
    declared: Option<u64>,
    /// The nesting gate's declared burst, once its `done` has been read. Read
    /// the same way, for the same reason.
    nest: Option<u64>,
    records: u64,
    reads: u64,
    /// Producer records — a storm's or the nesting burst's — this reader took.
    producer_records: u64,
    /// Producer records read **strictly before the last batch that carried
    /// one**, which is exactly "records this reader took while the producers
    /// were still emitting": a later batch carrying a producer record proves
    /// the workload had not finished when this one was read. **Zero would mean
    /// this reader raced nothing**, which is the one way a green conservation
    /// law says nothing at all.
    ///
    /// It needs no `done` and no clock, only the order of the batches.
    concurrent: u64,
    /// When the last batch carrying a producer record was read. `None` until
    /// one is, which is what leaves an ordinary boot's gate on the quiet reads
    /// alone.
    last_producer_at: Option<Instant>,
    /// Times the log's readiness source completed a poll. The `Source::Log`
    /// half of L4, asserted rather than assumed.
    completions: u64,
}

fn gate(cap: &SysCap) -> Result<(), String> {
    let mut tail = LogTail::new();
    let mut buf = [Record::EMPTY; BATCH];
    let mut run = Run {
        shards: [ShardLedger::default(); MAX_LOG_SHARDS],
        producers: BTreeMap::new(),
        storm: None,
        declared: None,
        nest: None,
        records: 0,
        reads: 0,
        producer_records: 0,
        concurrent: 0,
        last_producer_at: None,
        completions: 0,
    };

    // **Armed before the first read and kept armed**, which is what makes a
    // completion deterministic rather than lucky: the first read is what starts
    // the storm, so the records that answer this poll are committed after it was
    // registered, and re-arming after every harvest means a post landing *during*
    // the storm finds a pending poll rather than a gap.
    //
    // **It used to arm only on an empty read, and that made the assertion
    // depend on the shape of the boot.** During a storm no read is empty, so the
    // only poll in flight was the one from before the first read; whether it was
    // ever completed came down to when `klogd` happened to get a turn. At
    // `--smp 4` that measured `wakes=1`, and at `--smp 8` with `/bin/logd` also
    // reading the cursor it measured **zero** — a red about scheduling rather
    // than about the readiness source. `min_complete` 0 with no timeout submits
    // and harvests without blocking, so this costs one syscall a round.
    let poller = Poller::new(1);
    let mut armed = false;

    let mut quiet = 0u32;
    let started = Instant::now();
    loop {
        if !armed {
            poller.watch(cap, READABLE, LOG_TOKEN);
            armed = true;
        }
        poller.wait(0, 0, |token| {
            assert_eq!(token, LOG_TOKEN, "the log poll completed with another token");
            run.completions += 1;
            armed = false;
        });

        let batch = tail
            .read(cap, &mut buf)
            .map_err(|e| format!("SYS_LOG_READ refused a {BATCH}-record buffer: {e:?}"))?;
        run.reads += 1;
        if batch.is_empty() {
            quiet += 1;
        } else {
            quiet = 0;
            run.records += batch.len() as u64;
        }

        // **The concurrency evidence, from the order of the batches alone.**
        // Taken across the whole batch rather than per record: if this batch
        // carried a producer record, then everything this reader had taken from
        // a producer *before* it was taken while that producer was still
        // emitting. The last such batch is what fixes the number, so it is
        // assigned and not accumulated.
        let producer_records_before = run.producer_records;
        let shards = tail.shards();
        for record in batch {
            account(record, &mut run, shards)?;
        }
        if run.producer_records > producer_records_before {
            run.concurrent = producer_records_before;
            run.last_producer_at = Some(Instant::now());
        }

        // **The cursor decides, not a record.** Caught up, quiet for
        // `QUIET_READS` reads, and — once a producer has been seen — quiet for
        // `STORM_SETTLE` of guest time as well.
        let settled = run
            .last_producer_at
            .is_none_or(|at| at.elapsed() >= STORM_SETTLE);
        if quiet >= QUIET_READS && settled {
            break;
        }
        if started.elapsed() > CEILING {
            return Err(format!(
                "gave up after {:?}: {} records over {} reads, storm {:?}, {} producer record(s), \
                 {} producer(s) done",
                started.elapsed(),
                run.records,
                run.reads,
                run.storm,
                run.producer_records,
                run.producers.values().filter(|p| p.emitted.is_some()).count(),
            ));
        }
        if batch.is_empty() {
            // **Nothing new, so park on the readiness source rather than spin.**
            // `SYS_LOG_READ` never blocks by design; this is the other half of
            // that design, and the timeout is what bounds a machine that has
            // nothing left to say. The poll is already armed by the top of the
            // loop, so this parks on it rather than adding a second.
            poller.wait(1, IDLE_NANOS, |token| {
                assert_eq!(token, LOG_TOKEN, "the log poll completed with another token");
                run.completions += 1;
                armed = false;
            });
        }
    }

    // **The readiness source, observed deterministically rather than raced.**
    // Every completion above is a `klogd` post landing while this poll happened
    // to be pending, and during a storm that is a race against eight producers:
    // it measured `wakes=1` at `--smp 4` and **zero** at `--smp 8` once
    // `/bin/logd` was reading the cursor too, which is a red about scheduling
    // and not about `Source::Log`. So if the storm produced none, make one —
    // the shape `log_poll_outlives_a_close` already proves on this tree: a child
    // that runs and exits commits `process.rs`'s `exit:` line, which is one
    // kernel record from userland with no actuator and no privilege behind it.
    if run.completions == 0 {
        let mut child = std::process::Command::new("/bin/echo")
            .arg("log-gate")
            .spawn()
            .map_err(|e| format!("the record-making child would not start: {e}"))?;
        let _ = child.wait();
        if !armed {
            poller.watch(cap, READABLE, LOG_TOKEN);
        }
        poller.wait(1, READINESS_WAIT_NANOS, |token| {
            assert_eq!(token, LOG_TOKEN, "the log poll completed with another token");
            run.completions += 1;
        });
    }

    verdict(&tail, &run)
}

/// Put one record through both ledgers.
fn account(record: &Record, run: &mut Run, shards: u32) -> Result<(), String> {
    let cpu = record.cpu as usize;
    let ledger = run.shards.get_mut(cpu).ok_or_else(|| {
        format!("a record claims cpu{cpu}, past the ABI's {MAX_LOG_SHARDS} shards")
    })?;

    match ledger.first {
        None => ledger.first = Some(record.seq),
        Some(_) => {
            if record.seq < ledger.next {
                return Err(format!(
                    "cpu{cpu} answered seq {} after seq {}: a sequence number was read twice, \
                     or out of order, within one shard",
                    record.seq,
                    ledger.next - 1
                ));
            }
            ledger.gaps += record.seq - ledger.next;
        }
    }
    if record.at_ns < ledger.last_at_ns {
        return Err(format!(
            "cpu{cpu} seq {} is stamped {} ns, behind the {} ns of the record before it — within \
             a shard the sequence order is the timestamp order, and `emit` stamps inside the \
             same bracket it reserves in",
            record.seq, record.at_ns, ledger.last_at_ns
        ));
    }
    ledger.last_at_ns = record.at_ns;
    ledger.next = record.seq + 1;
    ledger.read += 1;

    let message = record.message();
    if message.len() != record.len as usize {
        return Err(format!(
            "cpu{cpu} seq {} declares {} message bytes and decodes to {}",
            record.seq,
            record.len,
            message.len()
        ));
    }

    // **What the batch-boundary concurrency evidence counts.** Every record
    // either of this machine's two workloads wrote, `start` and `done` records
    // included: the question it answers is "had the producers finished when
    // this batch was read", and a `done` is a producer still working as much as
    // a patterned record is.
    if message.starts_with("logstorm ") || message.starts_with("lognest ") {
        run.producer_records += 1;
    }

    if let Some(rest) = message.strip_prefix("lognest done ") {
        let emitted = rest
            .split_whitespace()
            .find_map(|w| w.strip_prefix("emitted="))
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| format!("`lognest done` is unreadable: {rest}"))?;
        if run.nest.replace(emitted).is_some() {
            return Err("the nesting gate said `done` twice".into());
        }
        return Ok(());
    }
    if message.starts_with("lognest ") {
        // `start` and `outer`. Both are records like any other and the burst
        // laps the shard they are in, so both are *expected* to be dropped —
        // which is the ring's declared policy and not a loss of evidence.
        return Ok(());
    }
    if let Some(rest) = message.strip_prefix("logstorm start ") {
        // Informative and cross-checked where it survives; never depended on.
        let (threads, records) = parse_start(rest)?;
        if threads != shards {
            return Err(format!(
                "the storm declared {threads} producer(s) on a machine of {shards} shard(s)"
            ));
        }
        run.storm = Some(shards);
        run.declared.get_or_insert(records);
        return Ok(());
    }
    if let Some(rest) = message.strip_prefix("logstorm done ") {
        let (thread, emitted) = parse_done(rest)?;
        run.storm = Some(shards);
        match run.declared {
            None => run.declared = Some(emitted),
            Some(declared) if declared != emitted => {
                return Err(format!(
                    "producer t={thread} emitted {emitted} records where another \
                     declared {declared}"
                ))
            }
            Some(_) => {}
        }
        let producer = run.producers.entry(thread).or_default();
        producer.mark_shard(record.cpu);
        if producer.emitted.replace(emitted).is_some() {
            return Err(format!("producer t={thread} said `done` twice"));
        }
        return Ok(());
    }
    let Some(rest) = message.strip_prefix("logstorm t=") else {
        // An ordinary kernel record. It is in the shard ledger above, which is
        // where the conservation law is computed; it declares nothing this gate
        // could regenerate.
        return Ok(());
    };

    let (thread, index) = parse_record(rest)?;
    let expected = storm_message(thread, index);
    if message != expected {
        return Err(format!(
            "cpu{cpu} seq {} is a torn or mixed storm record\n  read:     {message}\n  expected: {expected}",
            record.seq
        ));
    }
    // The nesting burst declares itself past every shard, so it is a producer
    // for the ledger's purposes and never one the storm is waiting on.
    if thread != NEST_PRODUCER {
        run.storm = Some(shards);
    }
    let producer = run.producers.entry(thread).or_default();
    producer.mark_shard(record.cpu);
    if let Some(next) = producer.next {
        if index < next {
            return Err(format!(
                "producer t={thread} answered index {index} after {}: one record's body was \
                 published under another record's sequence number",
                next - 1
            ));
        }
    }
    producer.next = Some(index + 1);
    producer.read += 1;
    Ok(())
}

/// The line a storm record carries, from the two numbers that identify it.
///
/// **The kernel builds this and the reader rebuilds it**, so a body half
/// overwritten by another generation fails on the byte that differs rather than
/// on a checksum that might not have covered it. `kernel/src/log/storm.rs` is
/// the other half; a disagreement between the two formulas reds loudly rather
/// than passing quietly.
fn storm_message(thread: u64, index: u64) -> String {
    let checksum = (thread.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ index.wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
    .rotate_left(17);
    let payload: String = (0..PAYLOAD)
        .map(|offset| (b'a' + (checksum.wrapping_add(offset as u64) % 26) as u8) as char)
        .collect();
    format!("logstorm t={thread} i={index} k={checksum:016x} {payload}")
}

fn parse_start(rest: &str) -> Result<(u32, u64), String> {
    let mut threads = None;
    let mut records = None;
    for word in rest.split_whitespace() {
        if let Some(v) = word.strip_prefix("threads=") {
            threads = v.parse::<u32>().ok();
        }
        if let Some(v) = word.strip_prefix("records=") {
            records = v.parse::<u64>().ok();
        }
    }
    match (threads, records) {
        (Some(t), Some(r)) => Ok((t, r)),
        _ => Err(format!("`logstorm start` is unreadable: {rest}")),
    }
}

fn parse_done(rest: &str) -> Result<(u64, u64), String> {
    let mut thread = None;
    let mut emitted = None;
    for word in rest.split_whitespace() {
        if let Some(v) = word.strip_prefix("t=") {
            thread = v.parse::<u64>().ok();
        }
        if let Some(v) = word.strip_prefix("emitted=") {
            emitted = v.parse::<u64>().ok();
        }
    }
    match (thread, emitted) {
        (Some(t), Some(e)) => Ok((t, e)),
        _ => Err(format!("`logstorm done` is unreadable: {rest}")),
    }
}

fn parse_record(rest: &str) -> Result<(u64, u64), String> {
    let mut words = rest.split_whitespace();
    let thread = words
        .next()
        .and_then(|w| w.parse::<u64>().ok())
        .ok_or_else(|| format!("a storm record names no thread: {rest}"))?;
    let index = words
        .next()
        .and_then(|w| w.strip_prefix("i="))
        .and_then(|w| w.parse::<u64>().ok())
        .ok_or_else(|| format!("a storm record names no index: {rest}"))?;
    Ok((thread, index))
}

/// The conservation law, and everything the gate prints for a reader of its
/// output.
fn verdict(tail: &LogTail, run: &Run) -> Result<(), String> {
    let seen: Vec<usize> =
        (0..MAX_LOG_SHARDS).filter(|&i| run.shards[i].first.is_some()).collect();
    if seen.is_empty() {
        return Err("no shard answered a single record".into());
    }
    if tail.shards() as usize != seen.len() {
        return Err(format!(
            "the kernel says this machine has {} shard(s) and {} answered a record",
            tail.shards(),
            seen.len()
        ));
    }

    // **`records_emitted == records_read + lost`, with the sequence numbers as
    // the ledger.** Every number a shard issued is either a record this reader
    // took or one it never saw, and the second is what the kernel derives
    // `lost` from — out of `head` and `next`, two numbers that have to be right
    // anyway, rather than out of a producer-side counter that could drift from
    // the ring.
    let mut computed = 0u64;
    for &i in &seen {
        let first = run.shards[i].first.expect("`seen` is the shards with a first record");
        computed += first - FIRST_SEQ + run.shards[i].gaps;
    }
    let reported = tail.lost();
    if computed != reported {
        let per_shard: Vec<String> = seen
            .iter()
            .map(|&i| {
                format!(
                    "cpu{i}: first={} last={} read={} gaps={}",
                    run.shards[i].first.unwrap_or(0),
                    run.shards[i].next.saturating_sub(1),
                    run.shards[i].read,
                    run.shards[i].gaps
                )
            })
            .collect();
        return Err(format!(
            "conservation failed: the sequence numbers say {computed} record(s) were never read \
             and the kernel counted {reported}\n  {}",
            per_shard.join("\n  ")
        ));
    }

    let mut emitted_total = 0u64;
    let mut read_total = 0u64;
    let mut migrated = 0u64;
    let mut said_done = 0u64;
    let mut unseen = 0u64;
    if let Some(threads) = run.storm {
        // **What every producer emitted, from a record where one survived and
        // from the ledger where none did.** `logstorm start` is written before
        // the first producer runs and each `done` after that producer's last
        // record; the storm laps every shard twice, so the ring is allowed to
        // drop any of them and this gate may not wait for one. The floor is the
        // highest index any producer reached — a producer emits `0..count`, so
        // the highest index seen plus one is a count no producer exceeded, and
        // the producer that finished last on a shard has its final records at
        // the newest end of it.
        let derived = run
            .producers
            .iter()
            .filter(|(&t, _)| t != NEST_PRODUCER)
            .filter_map(|(_, p)| p.next)
            .max();
        let declared = match (run.declared, derived) {
            (Some(declared), _) => declared,
            (None, Some(derived)) => derived,
            (None, None) => {
                return Err("storm records were read and none of them named an index".into())
            }
        };
        for thread in 0..threads as u64 {
            let Some(producer) = run.producers.get(&thread) else {
                // **A producer this reader never saw at all is the ring's
                // declared policy and not a failure**, and this used to be a
                // hard error. Two producers placed on one CPU write one shard,
                // and 1,024 records from the second lap all 1,024 of the first:
                // measured 2 of 7 full suites on the dev host, 2026-08-15, with
                // 2,582 records overwritten in a shard on the run that produced
                // it. Refusing it would be refusing the behaviour under test.
                //
                // It is not free either — see the ledger check below, which is
                // what stops "the reader saw nothing of it" from covering a
                // producer that never ran.
                unseen += 1;
                emitted_total += declared;
                continue;
            };
            // **A cross-check where the record survived, never a requirement.**
            // A producer whose `done` was lapped is a producer the ring
            // dropped a record of, which is the behaviour under test.
            if let Some(emitted) = producer.emitted {
                said_done += 1;
                if emitted != declared {
                    return Err(format!(
                        "producer t={thread} emitted {emitted} records against a declared \
                         {declared}"
                    ));
                }
            }
            if producer.read > declared {
                return Err(format!(
                    "producer t={thread} emitted {declared} records and this reader took {}",
                    producer.read
                ));
            }
            if producer.next.is_some_and(|next| next > declared) {
                return Err(format!(
                    "producer t={thread} answered index {} of a declared {declared}",
                    producer.next.unwrap_or(0) - 1
                ));
            }
            emitted_total += declared;
            read_total += producer.read;
            if producer.shards > 1 {
                migrated += 1;
            }
        }
        // **A producer nobody saw has to be one the ring dropped, and the
        // ledger is what says so.** `unseen` producers emitted `declared`
        // records each and none of them was read, so at least that many
        // sequence numbers must be among the ones the kernel counted lost. It
        // is a necessary condition rather than an attribution — the cursor's
        // `lost` is per shard and does not name producers — and it is what
        // separates "the ring lapped its whole run", which is the behaviour
        // under test, from "that thread never ran", which is a kernel that did
        // not spawn what it said it did.
        if unseen > 0 {
            let owed = unseen * declared;
            if reported < owed {
                return Err(format!(
                    "{unseen} producer(s) emitted {declared} record(s) each and this reader took                      none of them, while the kernel counted {reported} lost in all — a producer                      can only be invisible because its records were dropped, and the ledger does                      not account for the {owed} that would take"
                ));
            }
        }
        if read_total == 0 {
            return Err("the storm ran and this reader read none of it".into());
        }
        if run.concurrent == 0 {
            return Err(
                "every record was read after the storm had finished, so this reader raced nothing"
                    .into(),
            );
        }
        // The readiness source, asserted where it is reachable: the poll was
        // armed before the read that starts the storm, so the records that
        // answer it were committed after it was registered.
        if run.completions == 0 {
            return Err(
                "the log's readiness source completed no poll — not across the storm, and not on \
                 the record a child's exit commits afterwards either"
                    .into(),
            );
        }
    }

    if let Some(burst) = run.producers.get(&NEST_PRODUCER) {
        // The burst's own `done` is read the same way a storm's is: a
        // cross-check where it survived, and the ledger's own floor where it
        // did not. The burst laps its shard by construction (§9.2), so a reader
        // that required that record would be requiring one the design says may
        // go.
        let declared = match (run.nest, burst.next) {
            (Some(declared), _) => declared,
            (None, Some(next)) => next,
            (None, None) => {
                return Err("the nesting burst was seen and named no index".into())
            }
        };
        if burst.read == 0 {
            return Err("the nesting burst was injected and none of it was read".into());
        }
        if burst.next.is_some_and(|next| next > declared) {
            return Err(format!(
                "the nesting burst answered index {} of a declared {declared}",
                burst.next.unwrap_or(0) - 1
            ));
        }
        println!(
            // `nest_shards` and not `shards`: the line below reports the
            // machine's shard count under that name, and two lines defining one
            // name is a host-side reader that silently takes whichever came
            // last (`tests/common/logread.rs`).
            "log-gate: nest declared={declared} read={} dropped={} nest_shards={}",
            burst.read,
            declared - burst.read,
            burst.shards,
        );
    }

    println!(
        "log-gate: {} record(s) over {} read(s) from {} shard(s); lost={reported}, and the \
         sequence numbers say the same",
        run.records,
        run.reads,
        seen.len()
    );
    if run.storm.is_some() {
        // `done=` is the count of producers whose own `done` record survived
        // the ring, and it is evidence rather than an assertion — the gate no
        // longer waits for one and the number is what says how often the ring
        // ate one. Bare, not `k/n`: the host's reader takes the denominator of
        // an `a/b` field as the producer count and two of those would collide
        // (`tests/common/logread.rs`).
        println!(
            "log-gate: storm emitted={emitted_total} read={read_total} dropped={} \
             concurrent={} migrated={migrated}/{} done={said_done} unseen={unseen} wakes={}",
            emitted_total - read_total,
            run.concurrent,
            run.producers.len(),
            run.completions,
        );
    }
    println!("log-gate: OK");
    Ok(())
}
