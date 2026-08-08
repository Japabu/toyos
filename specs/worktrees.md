# Worktrees

One agent, one working tree, one branch. The shared tree was the only thing
several agents ever had to share, and most of the workflow rules in CLAUDE.md
exist to make sharing it survivable. This removes the reason for them.

Everything below was measured on the dev laptop (14 cores, Darwin 25.5.0,
git 2.50.1) on 2026-08-03.

## 1. What is per-worktree and what is not

| | where it lives | who writes it |
|---|---|---|
| working files, index, `HEAD`, branch | the worktree | its agent |
| every `target/` — root, `kernel/`, `bootloader/`, `userland/`, `toyos-ld/`, `toyos-cc/`, per-program | the worktree | its builds |
| `target/stamps/`, boot images, `nvme.img`, staged artifacts | the worktree | its builds |
| `.build-locks/` | the worktree | its builds |
| object store, refs | `<primary>/.git` | git |
| `rust/` — source, `build/`, the sysroot | the primary checkout | the primary checkout |
| `~/.rustup/toolchains/toyos` | the machine | the primary checkout |
| `<primary>/.git/toyos-build-locks/` | the repository | every worktree |

The root of every path a build touches is `env!("CARGO_MANIFEST_DIR")`, fixed
when that worktree compiled its own copy of the build system, so the first block
is per-worktree with no work. **The clean/build ENOENT race is gone by
construction** — two worktrees cleaning their own target directories are not
operating on the same files, and there is nothing left to serialise.

## 2. The toolchain has an owner, and the owner is derived

`rust/` is 50 GiB here, 47 GiB of it `build/`, and `~/.rustup/toolchains/toyos`
is one machine-global symlink into it. Nothing about that is per-worktree, so
nothing here copies it.

**Who owns it is asked of git, not recorded.** `git rev-parse --git-common-dir`
answers with the primary checkout's `.git` from anywhere in the repository, so
its parent *is* the primary checkout. A pointer file would be one more thing
that can disagree with the tree; this cannot go stale because it is not stored.

`toolchain::ensure` therefore splits: the primary runs the steps that write the
shared tree, and a linked worktree takes an early return that only *checks*.
`link_stale` is reached from the primary alone. **A non-owner finding the rustup
link pointing elsewhere is reading correct state, not staleness** — which is the
whole of `specs/test-cost-audit.md` §4.1 constraint 1.

`rust/` in a linked worktree stays the empty stub `git worktree add` leaves.
Two things were tried and rejected:

- **Initialising the submodule.** Git gives a linked worktree its own submodule
  git directory under `.git/worktrees/<name>/modules/` and shares no objects
  with the one already on disk: a 913 MiB, 3,434,185-object clone from GitHub,
  80 s and 1.4 GiB before anything is compiled — and that is before
  `library/backtrace` and `src/llvm-project`.
- **A symlink in its place.** Git refuses a symbolic link where a gitlink
  belongs and fails the whole command, not just that path:
  `error: expected submodule path 'rust' not to be a symbolic link` out of
  `status`, `submodule`, and anything else that walks the index. The worktree
  stops being usable as a worktree.

The empty stub is the one arrangement git is content with — `git status` is
clean and `git submodule status` reports it uninitialised, which it is.

## 3. The sysroot says what it was built from

std links `toyos-abi` and `toyos`; `libtoyos_c.a` is `userland/libc`. Three
**per-worktree** source trees are compiled into **one shared** sysroot.

A worktree whose `toyos-abi` differs from the sysroot's still compiles, still
links, and still boots — into a guest whose syscall arguments land at the wrong
offsets. No test catches it, because every test builds the same wrong thing.

So `rust/build/toyos-sysroot-witness` records the content of those three trees,
and every checkout compares before it builds. It is keyed on content and
repository-relative path where `stamps` uses mtime and absolute path, because
two checkouts of one commit agree on neither of the latter. The same change
fixes a defect the mtime stamps had *within* one tree: `git checkout` rewriting
an unchanged `toyos-abi` file used to buy a full std rebuild.

- **Match** — build.
- **Mismatch, linked worktree** — refuse, naming which of the three trees
  differs, **who built the sysroot and how long ago**, and what to do. Merging
  the change already in the sysroot is the ordinary answer, and
  `rust/build/toyos-sysroot-claimant` is what makes "merge it" an instruction
  somebody can follow: every process that writes the witness writes that beside
  it.
- **Mismatch, `--claim-sysroot`** — rebuild the shared sysroot from this
  worktree, under the global exclusive lock. Every other worktree then refuses
  until it merges. That is not a side effect to be sorry about: they genuinely
  cannot build correctly against it, and today they would have done so silently.
- **No record at all** — the primary records what is on disk; a linked worktree
  refuses. Absence is ignorance, not disagreement, and only the primary has the
  standing to resolve it. Verified when this landed: all three stamps it
  replaces were newer than their newest source, so the sysroot had been built
  from the sources beside it, and the first build after the change did no
  toolchain work.

An ABI change is therefore ordinary work in a worktree. It is the *landing* that
is global, and it announces itself.

Both refusals were run and say what they should. `--claim-sysroot`'s *act* was
not: it calls `rebuild_std` and `libc::build` unchanged — the primary's own daily
path — but running it once would have rebuilt the shared sysroot and, through
`external_fingerprint`, cleaned every other agent's target directories mid
session. The unexercised code is the `act_if` around those two calls. Worth
watching the first time it is used in anger.

### 3.1 A claim is an acquisition, not a rewrite

**A claim may not land inside another worktree's running gate**, and that is the
one decision `--claim-sysroot` shipped without. It could, and on 2026-08-04 it
did: the witness was rewritten at 23:03, 23:15, 23:27 and 23:41 — a ~13-minute
cadence — one agent's ABI change and another's claim ping-ponging through six
landing attempts at ~88 s each, a third agent's gate dying mid-run with 156
sysroot refusals, and a fourth parked for hours, able to land only once every
other agent had stopped. The refusal itself is right and stays. What it lacked
was somewhere to queue.

So the claim goes through the lock machinery. `buildlock::claim_sysroot` takes
the **sysroot lock** exclusively; every suite run takes it shared for its whole
length (`buildlock::run_against_sysroot`, once, at the top of `tests/toyos.rs`'s
`main`). A claim therefore waits for every run in flight, and `acquire`'s intent
file — the same writer preference the build lock has, and for the same measured
reason — makes every run that starts while it waits queue behind it, so a tree
running 15-25 suites a day cannot starve it.

Three things follow, and all three are constraints rather than preferences:

- **The sysroot lock is always outermost.** Sysroot → global, never the reverse,
  at every acquirer of both. `build()` takes the claim before
  `buildlock::shared`, and the harness takes the run lock before it compiles
  anything.
- **It is taken once per process.** A second acquisition where the process
  already holds it shared is a cycle with a queued claim's intent.
- **`--land` does not take it; its gate does**, because the gate is a separate
  `cargo test` process and a landing holding the lock while its own gate queued
  behind a claim would be exactly that cycle. What the landing leaves
  unprotected is the merge and the fast-forward, neither of which reads a
  sysroot.

Why not the `Scope::Global` lock a build already takes? Because it is the wrong
*length*. It says "nothing replaces the sysroot while I build", and a suite is a
hundred builds over two minutes, every one of them reading the sysroot the first
one agreed with. Holding it for the run instead would deadlock the run against
itself the moment one of its own builds escalated to the exclusive mode of a
file the process already held shared — the same argument that gave `integration`
its own file.

Two worktrees that both legitimately need the sysroot still take turns; that is
inherent to one shared sysroot and two incompatible ABIs, and only unsharing it
removes it. What changed is that a turn is now complete and announced: no gate
dies mid-run, a claim says before it starts whose sysroot it is taking, and a
suite that starts during one waits with a message naming the holder instead of
failing a hundred times.

Gates in `src/buildlock.rs`: `a_claim_waits_for_a_run_in_flight`,
`a_run_queues_behind_a_waiting_claim`, `a_killed_run_does_not_wedge_the_claim`.
All three are red when the run lock is taken on a directory the claim does not
name — which is the tree as it stood.

### 3.2 Who may claim: standing, and the fight that has no winner

Arbitration decides *when* a claim happens. It does not stop the wrong checkout
from making one, and on 2026-08-04 that was the whole event.

**Five of the six refused worktrees were byte-identical to main** in
`toyos-abi/src` and `toyos/src`. The sixth — the compositor agent's — held a
real change to the SDK's `SharedMemory` signature, fixing a bug that kills the
owner's desktop, and was the sole rightful holder. Every one of the five read
the refusal's own last sentence, "pass `--claim-sysroot`", as the way out. A
claim from a checkout that matches main rebuilds the sysroot **from main's
sources** — which is what all five already had — and refuses the one checkout
that cannot merge its way out. So the holder claims back. That is not a race to
be arbitrated; it is a fight whose winner is whoever ran most recently, and it
cost six landing attempts, four witness rewrites in 38 minutes, a gate dead with
156 refusals, and an agent parked for hours.

**So a claim now requires standing: this checkout's witnessed trees must
actually differ from main's**, committed and uncommitted alike (`git diff main`
over `SYSROOT_SOURCES`, which is also how the holder was identified that day).
Three answers, all three stated:

- **Diverged** — the checkout that cannot merge its way out. It may claim, and
  it is told to land as soon as it can, because nobody else can end the refusal
  from their end.
- **Matches main** — refused, with the holder named and **no `--claim-sysroot`
  in the message**. What it is told instead is the fact that makes waiting
  correct: when the holder lands, main carries what the sysroot holds and the
  refusal ends by itself, with nobody acting.
- **Unknown** — git could not answer. Refused, because a claim is destructive
  and an unanswered question is not permission.

**The primary's ordinary build was the same act, silently.** `std_sources_stale`
is true for the toolchain's owner in exactly two situations: its own sources
changed, or a worktree claimed the sysroot for something not on main yet. The
second is a lease, and rebuilding over it takes the sysroot from the one
checkout that cannot merge its way out. Watched live on 2026-08-05 — the witness
rewritten at 00:23, 00:26 and 00:47 while a worktree with a real SDK change and
a build in the primary took it from each other, poisoning two full suite runs
into 500.9 s and 427.5 s of nothing but refusals. The primary now refuses too,
telling the two cases apart by who wrote the witness, and `--claim-sysroot`
still takes it back — deliberately, and out loud.

**What none of this removes is the sharing.** One sysroot serves N worktrees, so
a checkout with a real ABI change still takes a turn during which the others
wait. Two things would remove it and neither was taken today:

- **A private sysroot for the diverged checkout.** Everything in stage2 except
  `lib/rustlib/x86_64-unknown-toyos` is ABI-independent, so a worktree that
  diverges could hold its own copy of that one directory with a
  `rustup toolchain link toyos-<worktree>` beside it, leaving the shared sysroot
  permanently main's. Nobody would ever be refused and nobody would ever claim.
  The cost is that `+toyos` is named at every cargo invocation in `src/` and in
  `tests/common/`, and that x.py writes into `rust/build` and would have to be
  copied out of it — a change that cannot be validated without a working
  toolchain, which is exactly what an agent in this situation does not have.
- **Landing the ABI change first**, which is cheaper and which the standing rule
  now points every refused checkout at.

**That mitigation is in the refusal itself now, because on 2026-08-07 the turn
was measured twice and both times it was a whole task long.** Eight worktrees on
the host, two of them waiting correctly rather than claiming: about 35 minutes in
one case and about 50 in the other, during which neither could build at all. One
of the two holders later reported having held the sysroot through two failed
gates before landing, where landing at the first clean boundary would have ended
the refusal for everybody. **The window is the claimant's to make small**: the ABI
half of a change is usually a few lines that compile on their own, and landing it
by itself makes the window one landing instead of one task. Applied successfully
once that day. `toolchain::CLAIM_WINDOW` is that sentence, printed by the refusal
a diverged checkout gets and again by the claim announcement — an agent in this
situation is reading the refusal, not this file.

**And the rule is now enforced at the cause rather than at the victim.** On
2026-08-07 it was followed once in four times: `toyos-vfserr` split deliberately
and said so; `toyos-memsec2` did not and held through two failed gates, blocking
two agents ~35 and ~50 minutes; `toyos-h3` did not, and two landings failed
against it in one hour — a log-audit branch at 129.6 s and a `tone` change at
94.8 s, each burning a full build before reaching `toolchain.rs`'s refusal. That
refusal is a good one arriving at the wrong process. `--land` now asks the same
question of the *landing* branch, in `preflight`, before the integration lock and
before anything is compiled: a branch whose commits touch the witnessed trees
**and** carry commits that touch none of them is refused, naming both lists.

It is a refusal and not a warning, because a warning is what the briefs already
were. The way past it is `--abi-inseparable`, for the case the split genuinely
cannot be made — an ABI item the branch renames or removes, whose old form the
rest of the tree still uses — and it is not silent: the report and the landing
commit both say the claim window was the whole task. Where the sysroot commits
are the *oldest* on the branch the refusal prints the two-command remedy;
where they are interleaved it says so, because nothing in this workflow rebases.
A previous landing attempt's integration merge is not unrelated work — merges are
excluded, or a branch whose only commit is the ABI change would be refused for
having tried once already. Gates:
`an_abi_change_landing_with_unrelated_work_is_refused_before_the_gate`,
`an_abi_only_branch_lands_and_a_previous_merge_is_not_unrelated_work`.

**A checkout that is merely *behind* main is not diverged from it**, and until
2026-08-07 `standing()` could not tell the two apart: it asked `git diff main`,
which is symmetric, so a worktree that had not merged somebody else's landed ABI
change read as `Diverged` and could claim — rebuilding the shared sysroot from
sources *older* than main's and refusing the checkout whose change had already
landed. It asks `git diff main...HEAD` against the merge base now, plus
`git status` for the working tree, which also catches an untracked file in
`toyos-abi/src` that no diff against a commit could see. Gate:
`a_checkout_behind_main_has_no_standing_to_claim`.

## 4. Two lock scopes

`buildlock::Scope` is named at every `act_if`, because the two are not
interchangeable in either direction: a toolchain phase in the worktree scope
serialises nothing across worktrees, and a target-directory clean in the global
scope stalls builds that cannot see it.

- **`Scope::Global`** — `<common-dir>/toyos-build-locks/`. The bootstrap, the
  hosted rustc, the rustup link, the host-target symlink, the toyos-libc
  install, the witness. Inside `.git/`, which the build system never cleans.
- **`Scope::Worktree`** — `<root>/.build-locks/`. The crate-target cleans,
  `toyos-ld`, `toyos-cc`, artifact staging.

A build holds **both** shared for its whole length: it reads the shared sysroot
from beginning to end, so a bootstrap in another worktree may no more land
inside it than a clean in this one.

**Both shared guards go down before either exclusive one is taken.** Holding one
while queueing for the other deadlocks against the process doing it the other
way round, and two builds in one worktree are that pair.

## 5. Integration

Agents commit freely on their own branch. Landing is explicit, serialized, and
never rewrites history.

In the worktree, at task end, `cargo run -- --land`:

1. Take the integration lock — exclusive.
2. `git merge --no-ff main`. Conflicts are resolved *here*, where the agent can
   build and test the merged result. This merge commit is the landing record and
   it names both parents.
3. Run the gate the change deserves.
4. `git -C <primary> merge --ff-only <branch>`.
5. Release.

`--ff-only` cannot silently take anything: step 2 already put main into the
branch, so a non-fast-forward means someone landed in between. Under the lock
that cannot happen, which makes the flag a check on the lock rather than a
fallback.

**The command is `src/land.rs`, and it is the whole protocol.** The lock is
`integration` in `<common-dir>/toyos-build-locks/`, taken exclusively for the
length of one landing and by nothing else. A distinct file from the `state` that
builds take in `Scope::Global`, because step 3's gate *is* a build and would
otherwise queue behind its own holder — `a_landing_and_a_build_do_not_exclude_each_other`
is the gate on that. No `intent` beside it: writer preference exists to keep a
stream of shared acquirers from starving an exclusive one, and nothing takes
this file shared at all.

What it refuses rather than guesses at:

- **A conflict is left in the worktree**, not aborted — the index and the markers
  git has already written are what the agent resolves against, and an abort
  deletes exactly those. The lock goes down first. Resolve, `git add`,
  `git commit`, re-run `--land`; a *next* run that finds `MERGE_HEAD` says so
  instead of merging over it.
- **main moving under the lock is reported as a bypass**, by name, listing what
  arrived, with nothing merged. Only `--land` takes the lock, so a main that
  moved between step 2 and step 4 means someone landed without it.
- **An uncommitted tree on either side.** The branch's, because the gate measures
  the working tree while main gets the commits; the primary's, because step 4
  moves it.

**The gate is `cargo test`**, the whole suite. `--gate <program> [args...]`
overrides it and takes the rest of the command line, so it comes last; any
override is printed before the gate runs and again in the report, because a
landing that gated less than the default must not be able to look like one that
did not.

**Both landings before the command paid for its absence.** macOS ships no
`flock` CLI, so nothing outside this build system can take a lock at all: one
agent reached for Python, and during the first landing's 10-minute gate another
agent put three commits on main. What caught that was `--ff-only`, exactly as
above — a check on a lock nobody was holding. Steps 2-4 were redone.

**And the third landing was `--land`'s own, which caught the same thing on its
first run.** Six commits — the Swiss German keyboard work, `a5c26b6..8e3f76d` —
went onto main during its 870 s gate, by hand, because on main the command did
not exist yet. The bypass report named all six and merged nothing; step 2 and
step 3 were run again against the new main. That is the last landing that can
happen, since `--land` is on main from that merge onward.

The redo is cheap only when the code delta is provable: `git diff <gated>..HEAD`
after the second merge showed markdown alone, so the 233-test run carried and
only the audio family was re-run before the fast-forward, both inside one hold.
A merge that moves code has no such shortcut and pays the full gate again.
`--land` has no such shortcut either — it re-gates in full.

**The gate runs inside the lock.** That queues landings, and the queue is the
honest cost of a gate that means something: CLAUDE.md's concurrent-measurement
rule says a suite perturbed by five other agents' QEMU boots is not evidence.
The optimistic alternative — gate outside, take the lock, re-check, re-gate if
main moved — is strictly faster and available if the queue ever bites, at the
cost of sometimes gating twice.

**The queue has to be audible, and on 2026-08-07 it was not.** Eight `--land`
processes were on this lock at once. Each printed one `[build-lock] waiting …`
line and then said nothing for as long as the seven ahead of it took, which is
what a wedge looks like — and an agent that kills a wedge puts its own gate back
at the end of the queue. Every blocking acquisition in `src/buildlock.rs` now
repeats itself every 30 s with the holder re-read each time, so the message
follows the queue forward instead of naming whoever was in front when the wait
began. The kernel still keeps the queue and a thread does the talking: nothing
polls a lock, so `flock`'s own ordering is given up nowhere. Gate:
`a_lasting_wait_keeps_saying_so`.

**The landing commit is written by `--land` and reads from main's side.** Step 2
creates a merge whose default message is `Merge branch 'main' into
wt/toyos-<x>` — the direction nobody reads. Measured 2026-08-07: **166 commits on
main in twenty hours, 66 of them that one sentence**, which makes
`git log --oneline` close to useless as a summary. The subject is now
`wt/toyos-<x> landed: N commits, gate ok` (or `gate ok, NOT clean`), and the body
carries the gate command, its duration and the suite's own last line quoted.

Two of those are things a reader cannot recover afterwards and are the reason the
commit carries them rather than the report alone: **which gate ran**, because
`--gate` overrides the default and one override that day silently narrowed to a
single test, and **whether the run was clean**, because `--land` accepts a run
carrying declared expected failures and only the suite says so.

The direction of the two parents is unchanged and nothing is squashed — the
history is honest and only its labelling was wrong. **Read it with
`git log --no-merges`** when you want the work rather than the landings.

Composing the message needs the gate's verdict, which does not exist when the
merge is made, so the merge is `--no-ff --no-commit` and the commit happens
between the gate and everything after it. Two consequences worth stating:

- **A failed gate still commits the merge**, with a subject saying the landing
  did not complete and claiming no verdict. Leaving it uncommitted would make the
  next `--land` report an unresolved conflict that is not there.
- **`toolchain::standing` ignores the working tree while a merge is in
  progress.** For the length of the gate the branch holds main's changes staged
  and uncommitted, which `git status` cannot tell from local work — and a build
  inside that gate would otherwise be told it had standing to claim a sysroot it
  has no delta for. What the branch has of its own is the `main...HEAD` question.
  Gate: `a_landing_s_uncommitted_merge_is_not_standing`.

**A landing writes its own log**, `target/landings/landing-<epoch>-<pid>.log`,
created with `O_EXCL` and named in the report. The gate's stdout and stderr are
copied to this process's own streams *and* into it, so the terminal still sees
the gate live. This is not a convenience: on 2026-08-07 two agents had a peer's
`--land` output interleave into their own redirected captures — foreign
`Compiling …` lines, foreign test failures — because the scratch directory an
agent redirects into is shared between concurrent sessions. One chased two
phantom red tests; one lost three landing attempts and ended up piping its
stream through `sed` to prefix every line. **A shell redirect cannot fix that,
because the two shells agree on the path**, so the fix has to be a name the
landing chooses. Do not redirect `--land`; read the file it names.

**Who merges: the agent, at task end.** The current model's one real virtue is
that agents build on each other's landings immediately, and an orchestrator
merge queue gives that up — branches diverge further the longer they wait, and
the person merging has the least context about the change. The agent has the
most. The orchestrator's leverage is *before* the merge, reviewing changes to
files that govern other agents (CLAUDE.md, specs that others read); that is a
review gate, not a merge queue.

**Merging into `main` needs the primary's tree clean**, since step 4 moves it.
The primary is therefore not an agent's workspace. It owns the toolchain, holds
`main`, and is where landings arrive.

## 6. The host is still one host

**Worktrees change nothing about the host, and CLAUDE.md's
concurrent-measurement rule is about the host.** Cores, disk bandwidth, and the
audio timing that gate A is calibrated on are shared by every worktree exactly
as they were shared by every agent.

- **Cores: 14.** Intra-suite parallelism and inter-worktree parallelism spend
  the same budget (`specs/test-cost-audit.md` §4.1 constraint 3). Four agents
  each running a 12-wide suite is 48 QEMUs on 14 cores — slower than serial and
  mismeasuring everything. **Built:** `buildlock::guest_slot` is that counting
  semaphore, `HOST_GUESTS` = 12 lock files in the global lock directory, one
  slot per task and never per boot. `specs/test-cost-audit.md` §5.6 has the
  mechanism and the four measured numbers. **And it counted the wrong thing on
  its own**: a worker holds its guest slot from the moment it takes a task, and
  the first part of that task is compiling a kernel variant, so twelve workers
  are twelve concurrent `cargo build`s and no guest at all — load 49.9 on
  fourteen cores with one guest live, measured 2026-08-07.
  `buildlock::build_slot` is the second count, four across every worktree,
  `specs/test-cost-audit.md` §5.7.
- **Gate A.** `tests/audio-baseline.toml`'s numbers were recorded with one QEMU
  at a time and no concurrent agents. Worktrees make that condition rarer, not
  differently rare — six agents in one tree already broke it. The options and
  their costs are `specs/test-cost-audit.md` §4.1 constraint 4. **The owner
  ruled on 2026-08-04: no measurement locks and no quiet-host scheduling**, so
  gate A takes one slot like everything else and reserves nothing. Its fast
  tier's verdict is harm — a gap, an underrun, a dropout — and a load-coincident
  failure is investigated as a real defect rather than re-run away.
- **Fixed `/tmp` paths** — `/tmp/toyos-qmp.sock`, `/tmp/toyos-audio.wav`,
  `/tmp/toyos-qemu-debug.log`, `/tmp/toyos-debug-*` — collide across worktrees.
  All of them are on the `cargo run` and interactive-debug paths, which agents
  do not use. The test harness already keys its scratch on pid
  (`toyos-tests-{pid}`), so `cargo test` in two worktrees does not collide.

## 7. What it costs

```
cargo run -- --worktree add <path>       1 s,   23 MiB
  ... versus git worktree add + cargo run:
                                        80 s,  1.4 GiB, then a full bootstrap

per worktree, after --build-only              4.1 GiB
per worktree, having run everything            23 GiB   (the primary's own)
shared once, never copied         rust/        50 GiB   (47 GiB of it build/)
```

So N worktrees cost `50 + 23N` GiB at worst, not `73N`. On 111 GiB free that is
two comfortably and three at the edge, against exactly one the naive way.

Two cold worktrees built concurrently in 184 s and 180 s, with no bootstrap, no
submodule clone, and no `[build-lock] waiting` line in either: their target
directories do not overlap and the toolchain phases had nothing to do.

## 8. The setup command

`cargo run -- --worktree add <path>` — creates the worktree on `wt/<name>` from
`main`, carries over `.cargo/config.toml` (gitignored, and a worktree that
silently lost the fork redirects would build different code and report the
difference as a result), and prints where the shared toolchain is.

It refuses, by name and before creating anything, when the toolchain does not
exist yet or the disk is short. A half-made worktree is worse than none, because
the next agent finds it and believes it.

`--worktree list` shows the owner, the rustup link, and the worktrees.
`--worktree remove <path>` is deliberately not `--force`: work in a worktree is
the only copy of itself.

**A worktree checks out a commit.** Uncommitted work in the primary is not in
it — including, the first time this was tried, the build-system change that
makes worktrees work at all, which sent the new worktree straight into the
bootstrap this document exists to prevent.

**It carried `.cargo/config.toml` and not `assets/timgm6mb.sf2`**, which reddened
nine tests of the desktop and metal-sim families in every brand-new worktree
until somebody copied that file across. Closed 2026-08-08, and not by copying it:
no config that builds no doom declares a SoundFont any longer, and the one that
does declares an absence rather than a requirement. There is nothing left to copy
— `specs/ci-plan.md` §6 is the account.
