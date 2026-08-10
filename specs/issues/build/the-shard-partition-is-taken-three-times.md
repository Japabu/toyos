---
status: open
kind: defect
opened: 2026-08-10
---

# The shard partition is taken three times over, and the three imbalances add

`tests/toyos.rs` shards the run in three separate calls:

```rust
shard.keep(&mut parallel, cost);
shard.keep(&mut serial, cost);
shard.keep(&mut audio_to_run, ...);
```

and `Shard::keep` (`src/testargs.rs`) begins each of them with
`let mut load = vec![Duration::ZERO; self.count]`. So the parallel tasks are
balanced across twelve bins, then the serial tail is balanced across twelve
*empty* bins, then gate A's configs across twelve empty bins again. Each
partition is good; their sum is not, because the second and third know nothing
about what the first put where.

**Measured, and it is the whole of what is left.** Run `31377439504`
(`wt/toyos-ciperfect` at `4e6a5a6`) partitions on a profile refreshed from a run
of the same tree on the same machine class, so the prices it used are the times
it then took — per shard, `measured` is within 0.1 s of the wall clock except
where a failure bought a re-run:

| shard | measured s | wall s |
|---|---|---|
| 1 | 295.1 | 295.1 |
| 2 | 439.2 | 439.2 |
| 3 | 466.1 | 466.1 |
| 4 | 429.0 | 445.5 |
| 5 | 451.6 | 451.6 |
| 6 | 345.8 | 345.8 |
| 7 (the 179-test shared block) | 304.0 | 307.2 |
| 8 | 320.1 | 320.1 |
| 9 | 466.0 | 466.0 |
| 10 | 269.5 | 283.5 |
| 11 | 285.9 | 285.9 |
| 12 | 357.4 | 357.4 |

Sum 4,429.6 s, an even split of 369.1 s, and a widest shard of **466.1 s**:
**97 s of critical path with a profile that is exactly right**. Simulating the
harness's own LPT over the same items as *one* pool — the shared block as a
single indivisible task of 304 s, everything else on its own — puts the widest
bin at 363.9 s, three seconds off the even split. The profile is not the
constraint any more; the three calls are.

That number is paid on every merge, because `guest-suite` is a required check
and a run ends when its slowest shard does.

**The fix is one accumulator.** `keep` takes the load vector rather than
allocating it, the three calls thread one through, and the second and third
partitions fill the bins the first left light. It also wants the calls in
descending order of pool weight, which they already are.

Two reasons it was not done on the CI task of 2026-08-10: `keep`'s three call
sites are in `tests/toyos.rs`, which was outside that task's surface while a
large branch was finishing in it; and the arm that settles it is another full
twelve-shard run, which at the time was queueing behind that branch. The number
to beat is 466.1 s against an even split of 369.1 s, on a profile that is
already accurate — so the A/B needs no new measurement of anything else.

Beside it, and larger: `specs/issues/build/every-shard-rebuilds-the-whole-tree.md`
is 435 s of the same critical path.
