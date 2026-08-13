# Worktrees

One agent, one working tree, one branch. The shared tree was the only thing
several agents ever had to share, and most of the workflow rules in CLAUDE.md
exist to make sharing it survivable. This removes the reason for them.

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

`rust/` is 50 GiB, 47 GiB of it `build/`, and `~/.rustup/toolchains/toyos` is
one machine-global symlink into it. Nothing about that is per-worktree, so
nothing here copies it.

**Who owns it is asked of git, not recorded.** `git rev-parse --git-common-dir`
answers with the primary checkout's `.git` from anywhere in the repository, so
its parent *is* the primary checkout. A pointer file would be one more thing
that can disagree with the tree; this cannot go stale because it is not stored.

`toolchain::ensure` therefore splits three ways. The primary runs the steps that
write the shared tree. A linked worktree takes an early return that only
*checks*, and `link_stale` is reached from the primary alone: **a non-owner
finding the rustup link pointing elsewhere is reading correct state, not
staleness**, which is `specs/assessments/test-cost-audit.md` §4.1 constraint 1.
A checkout holding a toolchain that arrived as an artifact, with no `rust/`
source to have built it from, is `Owner::Installed` — it compares the same
witness and refuses `--rebuild-toolchain` and `--claim-sysroot` alike, because
there is nothing there to build either from.

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

std links `toyos-abi` and `toyos`; `libtoyos_c.a` is `userland/libc`. Those
three **per-worktree** source trees are `toolchain::SYSROOT_SOURCES`, and they
compile into **one shared** sysroot: for the length of one `x build`,
`rust/library/std/Cargo.toml`'s two path dependencies are retargeted at the
worktree doing the building, so a worktree's std holds that worktree's ABI
rather than main's.

A worktree whose `toyos-abi` differs from the sysroot's still compiles, still
links, and still boots — into a guest whose syscall arguments land at the wrong
offsets. No test catches it, because every test builds the same wrong thing.

So `rust/build/toyos-sysroot-witness` records the content of those three trees,
and every checkout compares before it builds. It is keyed on content and
repository-relative path where `stamps` uses mtime and absolute path, because
two checkouts of one commit agree on neither of the latter, and because
`git checkout` rewriting an unchanged file is not a change.

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
  cannot build correctly against it, and otherwise they would have done so
  silently.
- **No record at all** — the primary records what is on disk; a linked worktree
  refuses. Absence is ignorance, not disagreement, and only the primary has the
  standing to resolve it.

An ABI change is therefore ordinary work in a worktree. It is the *landing* that
is global, and it announces itself.

**A claim's act reaches every other worktree, not only the refusals around it.**
It runs `rebuild_std` and `libc::build` under the global exclusive lock, which
replaces the sysroot rlibs; `build::external_fingerprint` reads their mtimes, so
the next build in every other worktree cleans its crate targets. The refusals on
either side of the act are gated; the act itself is not.

### 3.1 A claim is an acquisition, not a rewrite

**A claim may not land inside another worktree's running gate.** The refusal §3
describes is correct; what it needs beside it is somewhere to queue.

So the claim goes through the lock machinery. `buildlock::claim_sysroot` takes
the **sysroot lock** exclusively; every suite run takes it shared for its whole
length (`buildlock::run_against_sysroot`, once, at the top of `tests/toyos.rs`'s
`main`). A claim therefore waits for every run in flight, and `acquire`'s intent
file — the same writer preference the build lock has — makes every run that
starts while it waits queue behind it, so a tree that always has a suite in
flight cannot starve it.

Three things follow, and all three are constraints rather than preferences:

- **The sysroot lock is always outermost.** Sysroot → global, never the reverse,
  at every acquirer of both. `build()` takes the claim before
  `buildlock::shared`, and the harness takes the run lock before it compiles
  anything.
- **It is taken once per process.** A second acquisition where the process
  already holds it shared is a cycle with a queued claim's intent.
- **Landing does not take it, and cannot.** `cargo run -- --pr` builds nothing:
  it is git, a merge and a push, none of which reads a sysroot. A landing
  holding it while its gate — a separate process — queued behind a claim would
  be exactly that cycle.

Why not the `Scope::Global` lock a build already takes? Because it is the wrong
*length*. It says "nothing replaces the sysroot while I build", and a suite is a
hundred builds over two minutes, every one of them reading the sysroot the first
one agreed with. Holding it for the run instead would deadlock the run against
itself the moment one of its own builds escalated to the exclusive mode of a
file the process already held shared — the same argument that gave `integration`
its own file.

Two worktrees that both legitimately need the sysroot still take turns; that is
inherent to one shared sysroot and two incompatible ABIs, and only unsharing it
removes it. What the lock adds is that a turn is complete and announced: no gate
dies mid-run, a claim says before it starts whose sysroot it is taking, and a
suite that starts during one waits with a message naming the holder instead of
failing every build it attempts.

Gates in `src/buildlock.rs`: `a_claim_waits_for_a_run_in_flight` — an exclusive
acquisition is refused while a run holds it shared, and a second *run* is not;
`a_run_queues_behind_a_waiting_claim` — the order is claim then run;
`a_killed_run_does_not_wedge_the_claim` — a SIGKILLed run frees it, because it
is `flock` and not a pid file.

### 3.2 Who may claim: standing

Arbitration decides *when* a claim happens. It does not stop the wrong checkout
from making one, and a checkout that matches main has nothing to claim *with*: a
claim from it rebuilds the sysroot **from main's sources**, which is what every
other worktree already has, and refuses the one checkout that cannot merge its
way out. That checkout then claims back. It is not a race to be arbitrated but a
fight whose winner is whoever ran most recently.

**So a claim requires standing: this checkout's witnessed trees must actually
differ from main's**, committed and uncommitted alike. `toolchain::standing`
asks `git diff main...HEAD` over `SYSROOT_SOURCES` — against the merge base,
because `git diff main` is symmetric and a checkout that has merely not merged
somebody else's landed ABI change is not diverged from it — plus `git status`
for the working tree, which also catches an untracked file in `toyos-abi/src`
that no diff against a commit could see. Three answers, all three stated:

- **Diverged** — the checkout that cannot merge its way out. It may claim, and
  it is told to land as soon as it can, because nobody else can end the refusal
  from their end.
- **Matches main** — refused, with the holder named and **no `--claim-sysroot`
  in the message**. What it is told instead is the fact that makes waiting
  correct: when the holder lands, main carries what the sysroot holds and the
  refusal ends by itself, with nobody acting.
- **Unknown** — git could not answer. Refused, because a claim is destructive
  and an unanswered question is not permission.

**A merge in progress is not this branch's statement about itself.** An agent
resolving `--pr`'s merge of main holds every file main changed as local work, so
the working-tree question is skipped while `MERGE_HEAD` exists and only the
committed one answers.

**The primary's ordinary build is the same act.** `std_sources_stale` is true
for the toolchain's owner in exactly two situations: its own sources changed, or
a worktree claimed the sysroot for something not on main yet. The second is a
lease, and rebuilding over it takes the sysroot from the one checkout that
cannot merge its way out. The primary refuses too, telling the two cases apart
by who wrote the claimant record, and `--claim-sysroot` still takes it back —
deliberately, and out loud.

**What none of this removes is the sharing.** One sysroot serves N worktrees, so
a checkout with a real ABI change still takes a turn during which the others
cannot build at all. Two things would remove it:

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
  points every refused checkout at.

**The window is the claimant's to make small.** The ABI half of a change is
usually a few lines that compile on their own, and landing it by itself makes
the window one landing instead of one whole task. `toolchain::CLAIM_WINDOW` is
that sentence, printed by the refusal a diverged checkout gets and again by the
claim announcement, because the refusal is what an agent in this situation is
actually reading.

**And the rule is enforced at the cause rather than at the victim.** The refusal
in `toolchain.rs` is a good one arriving at the wrong process: it reaches an
unrelated agent, after a full build, for somebody else's mistake. The same
question is asked of the *landing* branch before anything is compiled — a branch
whose commits touch the witnessed trees **and** carry commits that touch none of
them is refused, naming both lists. `pr::abi_lands_alone` is the one
implementation and it answers in two places: at `cargo run -- --pr` in a second,
and at CI's `abi-split` check for a branch that skipped the preflight, CI being
the only thing between a branch and `main`.

It is a refusal and not a warning. The way past it is an
**`Abi-Inseparable: <why>` trailer** in one of the branch's commit messages, for
the case the split genuinely cannot be made — an ABI item the branch renames or
removes, whose old form the rest of the tree still uses. A trailer is in the
branch's own history, lands with it, and is the one form CI can read at all,
since CI has no command line from the author. Where the sysroot commits are the
*oldest* on the branch the refusal prints the two-command remedy; where they are
interleaved it says so, because nothing in this workflow rebases. A branch's own
update merges are not unrelated work — merges are excluded, or a branch whose
only commit is the ABI change would be refused for having merged main once.

Gates: `a_checkout_behind_main_has_no_standing_to_claim`,
`a_checkout_identical_to_main_has_no_standing_to_claim`,
`an_abi_change_is_standing_committed_or_not`,
`a_landing_s_uncommitted_merge_is_not_standing` and
`git_that_cannot_answer_gives_no_standing` in `src/toolchain.rs`;
`a_branch_mixing_the_sysroot_with_dependent_work_is_refused`,
`the_inseparable_trailer_is_the_escape_and_it_is_in_the_history` and
`an_abi_only_branch_and_an_ordinary_branch_both_pass` in `src/pr.rs`.

### 3.3 A std edit is made in the primary's `rust/`, and it needs no claim

**Where it has to live.** A linked worktree's `rust/` is the stub §2 leaves and
`toolchain::rust_dir` sends every read to the primary's, so *the*
`rust/library` on this host is one directory and editing the std fork edits it
for every checkout at once. There is nowhere else to put the edit and no way to
hold it privately.

**What that does and does not cost.** It does not cost a claim.
`rust/build/toyos-std-fork-witness` is a hash of `rust/library` kept apart from
the other three witnessed trees, and a worktree finding only *that* stale
rebuilds std rather than being refused (`toolchain::std_fork_stale`) — because a
rebuild out of the shared `rust/` produces the sysroot every checkout wants and
takes nothing from any of them, where a `toyos-abi` claim refuses all of them
until it lands. What it does cost is that the next build in *every* worktree
links against an edit that has not landed yet, so a std change is landed
promptly for the same reason an ABI change is.

**Type-checking one without touching the sysroot at all** is worth having while
iterating, because the rebuild above is minutes and this is seconds.
`~/.rustup/toolchains/toyos/lib/rustlib/src/rust` is a symlink to the primary's
`rust/`, so `-Zbuild-std` compiles the working tree rather than a copy of it,
and it writes only into a target directory you name. `cargo` is not in the
toyos toolchain — rustup falls back to the default one's, which is fine, the
`rustc` is what matters:

    __CARGO_TESTS_ONLY_SRC_ROOT=<src> CARGO_TARGET_DIR=<scratch> \
      cargo +toyos build -Z build-std=std,panic_abort \
      --target x86_64-unknown-toyos --offline

`<src>` needs a `library/` and a root `Cargo.toml` whose workspace names
`library/std` — the real `rust/Cargo.toml` does not, its workspace is the
compiler's and `library/` has its own. Pointing `<src>` at an APFS clone of
`rust/library` (`cp -Rc`, 59 MiB, instant) under a directory holding symlinks to
`toyos-abi` and `toyos` — std names them `../../../toyos-abi` — gives a tree
that can be edited while the primary stays clean and no other worktree's build
sees anything.

Two things it does not tell you. Cargo does not re-fingerprint std on a source
change under `-Zbuild-std`, so delete `<scratch>/<target>/debug/.fingerprint/std-*`
between runs or you will read a stale green. And it is a compile, not a boot:
whether the guest behaves differently still needs the sysroot.

## 4. Two lock scopes

`buildlock::Scope` is named at every `act_if`, because the two are not
interchangeable in either direction: a toolchain phase in the worktree scope
serialises nothing across worktrees, and a target-directory clean in the global
scope stalls builds that cannot see it.

- **`Scope::Global`** — `<common-dir>/toyos-build-locks/`. The bootstrap, the
  hosted rustc, the rustup link, the host-target symlink, the toyos-libc
  install, the witness, and both std rebuilds §3 describes. Inside `.git/`,
  which the build system never cleans.
- **`Scope::Worktree`** — `<root>/.build-locks/`. The crate-target cleans,
  `toyos-ld`, `toyos-cc`, artifact staging.

A build holds **both** shared for its whole length: it reads the shared sysroot
from beginning to end, so a bootstrap in another worktree may no more land
inside it than a clean in this one.

**Both shared guards go down before either exclusive one is taken.** Holding one
while queueing for the other deadlocks against the process doing it the other
way round, and two builds in one worktree are that pair.

**The order across every lock in the module is a constraint:** sysroot → host
slot (guest or build, §6) → build lock → artifact. A build slot is taken after
the sysroot lock rather than before it, because a `--claim-sysroot` holds the
sysroot and then wants a build slot.

## 5. Integration

Agents commit freely on their own branch. Landing is explicit, serialized, and
never rewrites history. **It happens on GitHub.**

`cargo run -- --land` is retired and answers rather than going missing. The
reason is the gate rather than the mechanism — the dev host is arm64 emulating
x86, `specs/assessments/ci-plan-assessment-2026-08.md` §7 is a class of defect
it cannot execute at all, and a gate blind to a class is not a gate against it.
The gate is twelve KVM shards on x86_64, and the merge went with it.

In the worktree, at task end:

1. `cargo run -- --pr`. It refuses what would be wrong, fetches, fast-forwards
   this host's `main`, merges `origin/main` into the branch, and pushes it.
2. **`gh pr create --draft` on the first push**, which `--pr` prints when the
   branch is not yet on the remote. CI runs on a pull request and on nothing
   else, so a branch without one is ungated for however long it lives. Every
   later `--pr` updates the same pull request by pushing to its head.
3. `gh pr ready` when it is finished, with a written `gh pr edit --title` and
   `--body-file`. **Never `--fill`**: those two become the merge commit's
   message, and `--fill` concatenates the commits instead of stating what
   landed.
4. `gh pr merge --auto --merge`, and walk away. GitHub merges when the required
   checks pass.
5. `cargo run -- --sync` afterwards, or at the top of the next task, to bring
   this host's `main` up to what was merged.

**Step 1's merge is the load-bearing one and it is not a convenience.** The
required checks are *strict* — GitHub refuses the merge button until the branch
contains `origin/main` — so the checks that run on the branch head are checks on
the merged result. That is what catches a semantic conflict between two branches
that each pass alone, and it is the one property a naive "CI on the pull request"
setup throws away. GitHub's native merge queue is the feature that restores it
properly; it is **not available on this repository**, and
`specs/assessments/ci-plan-assessment-2026-08.md` §10.1 has the two API answers
that say so.

**Serialization comes from the same place.** The first merge moves `main`, and
from that instant every other open pull request is out of date and has to merge
again and re-run. That is the integration lock, enforced by the thing that
actually moves `main` rather than by an advisory `flock` on one laptop.

`specs/assessments/ci-plan-assessment-2026-08.md` §10.3 is the table of where
each of `--land`'s invariants went, §10.4 the two-stage switch and its trigger,
and §10.5 the repository settings none of this can carry in a diff.

### What `--pr` refuses rather than guesses at

- **A conflict is left in the worktree**, not aborted — the index and the markers
  git has already written are what the agent resolves against, and an abort
  deletes exactly those. Resolve, `git add`, `git commit`, re-run `--pr`; a run
  that finds `MERGE_HEAD` says so instead of merging over it.
- **An uncommitted tree.** CI gates what was pushed, so uncommitted work would be
  gated by nothing and merged by nothing.
- **`main` itself, and a detached `HEAD`.** A pull request needs a branch.
- **A branch mixing the shared sysroot's sources with work that depends on
  them** — §3.2's rule, refused here in a second and again by CI's `abi-split`
  check, both out of `pr::abi_lands_alone`. The escape is an
  `Abi-Inseparable: <why>` trailer in a commit message, which lands with the
  branch and stays as the record.
- **A branch carrying nothing `origin/main` does not already have.**
- **A push that is not a fast-forward.** Nothing here forces one. A pushed branch
  is a hash somebody's CI run may already have cited.

### The merge commit is the record, and it reads from main's side

git's default for a branch merge is `Merge branch 'main' into wt/toyos-x`, the
direction nobody reads. GitHub composes the message out of the pull request's
title and body instead (`merge_commit_title=PR_TITLE`,
`merge_commit_message=PR_BODY`), so the title is what `git log --oneline` shows
and `.github/pull_request_template.md` asks for the rest. **Read the history
with `git log --no-merges`** when you want the work rather than the landings.

Two facts the merge commit does not carry itself: **which gate ran** is the check
run on the head commit, durable and linked from the merge; **whether the run was
clean** is `EXPECTED_FAILURES` in `tests/toyos.rs`, which is in the diff being
reviewed, and every shard's own `test result:` line in the run's job summary.

### What is left of the integration lock

`buildlock::integration` survives with a narrower job: **one process at a time
moves this host's `main`**. `--sync` fast-forwards the primary checkout, and the
primary is a checkout somebody may be building in. It is housekeeping and not a
gate, so a primary that is dirty or on another branch is *reported* and left
where it is.

`origin/main` is the truth and this host's `main` is a cache of it. A local
`main` carrying commits `origin/main` has not cannot be fast-forwarded, and
`--sync` refuses rather than guessing: it lists what is stranded and which
branches already contain all of it, because "settle that" on its own sends an
agent to read the reflog.

**A queue has to be audible.** Every blocking acquisition in `src/buildlock.rs`
repeats itself every 30 s with the holder re-read each time, so a wait that
lasts does not look like a wedge. Gate: `a_lasting_wait_keeps_saying_so`.

### Who merges: the agent, at task end

The current model's one real virtue is that agents build on each other's
landings immediately, and an orchestrator merge queue gives that up — branches
diverge further the longer they wait, and the person merging has the least
context about the change. The agent has the most, and `gh pr merge --auto` is
the agent saying so and then leaving.

The owner's leverage is the pull request itself: changes to the files that
govern other agents — CLAUDE.md, the specs others read — are reviewable before
they land. **Zero approving reviews are required**, deliberately: one human and
several agents means a review requirement is a deadlock on him being awake.

**The primary checkout is not an agent's workspace.** It owns the toolchain,
holds `main`, and `--sync` moves its tree.

## 6. The host is still one host

**Worktrees change nothing about the host, and CLAUDE.md's
concurrent-measurement rule is about the host.** Cores, disk bandwidth, and the
audio timing that gate A is calibrated on are shared by every worktree exactly
as they were shared by every agent.

- **Cores: 14.** Intra-suite parallelism and inter-worktree parallelism spend
  the same budget (`specs/assessments/test-cost-audit.md` §4.1 constraint 3).
  Four agents each running a 12-wide suite is 48 QEMUs on 14 cores — slower than
  serial and mismeasuring everything. `buildlock::guest_slot` is that counting
  semaphore: `HOST_GUESTS` = 12 lock files in the global lock directory, one
  slot per task and never per boot, `specs/assessments/test-cost-audit.md` §5.6
  for the mechanism and the four properties that carry it. **And it counts the
  wrong thing on its own**: a worker holds its guest slot from the moment it
  takes a task, and the first part of that task is compiling a kernel variant,
  so twelve workers are twelve concurrent `cargo build`s and no guest at all.
  `buildlock::build_slot` is the second count — `HOST_BUILDS` = 4 across every
  worktree, §5.7.
- **Gate A.** `tests/audio-baseline.toml`'s numbers were recorded with one QEMU
  at a time and no concurrent agents. Worktrees make that condition rarer, not
  differently rare. The options and their costs are
  `specs/assessments/test-cost-audit.md` §4.1 constraint 4; **the owner has
  ruled that there are no measurement locks and no quiet-host scheduling**, so
  gate A takes one slot like everything else and reserves nothing. Its fast
  tier's verdict is harm — a gap, an underrun, a dropout — and a load-coincident
  failure is investigated as a real defect rather than re-run away.
- **Fixed `/tmp` paths** — `/tmp/toyos-qmp.sock`, `/tmp/toyos-audio.wav`,
  `/tmp/toyos-qemu-debug.log`, `/tmp/toyos-debug-*` — collide across worktrees.
  All of them are on the `cargo run` and interactive-debug paths, which agents
  do not use. The test harness keys its scratch on pid (`toyos-tests-{pid}`), so
  `cargo test` in two worktrees does not collide.

## 7. What it costs

```
cargo run -- --worktree add <path>       1 s,   23 MiB
  ... versus git worktree add + cargo run:
                                        80 s,  1.4 GiB, then a full bootstrap

per worktree, after --build-only              4.1 GiB
per worktree, having run everything            23 GiB   (the primary's own)
shared once, never copied         rust/        50 GiB   (47 GiB of it build/)
```

So N worktrees cost `50 + 23N` GiB, not `73N`. `--worktree add` refuses below
25 GiB free (`worktree::NEEDED_BYTES`), which is the upper figure plus a little:
a build that fills the disk halfway through costs more than a worktree that was
never made.

## 8. The setup command

`cargo run -- --worktree add <path>` — creates the worktree on `wt/<name>` from
`main`, carries over `.cargo/config.toml`, and prints where the shared toolchain
is. That file is the one thing a worktree needs that git does not carry: it is
gitignored, and a worktree that silently lost the fork redirects would build
different code and report the difference as a result. Everything else, the
SoundFont under `assets/` included, arrives with the checkout.

It refuses, by name and before creating anything, when the toolchain does not
exist yet or the disk is short. A half-made worktree is worse than none, because
the next agent finds it and believes it.

`--worktree list` shows the owner, the rustup link, and the worktrees.
`--worktree remove <path>` is deliberately not `--force`: work in a worktree is
the only copy of itself. It leaves the branch, and `git branch -d` is what says
whether it was merged.

**A worktree checks out a commit.** Uncommitted work in the primary is not in
it — a build-system change included, which sends the new worktree straight into
the bootstrap §2 exists to prevent.
