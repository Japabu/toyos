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
- **Landing does not take it, and now cannot.** `cargo run -- --pr` builds
  nothing: it is git, a merge and a push, none of which reads a sysroot. While
  the landing gate was a `cargo test` on this host, the rule was that `--land`
  itself had to stay out of the lock and let its gate — a separate process —
  take it, or the two were exactly that cycle.

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
refusal is a good one arriving at the wrong process. The same question is asked
of the *landing* branch now, before anything is compiled: a branch whose commits
touch the witnessed trees **and** carry commits that touch none of them is
refused, naming both lists. `pr::abi_lands_alone` is the one implementation and
it answers in two places — at `cargo run -- --pr` in a second, and at CI's
`abi-split` check for a branch that skipped the preflight, CI being the only
thing between a branch and `main`.

It is a refusal and not a warning, because a warning is what the briefs already
were. The way past it is an **`Abi-Inseparable: <why>` trailer** in one of the
branch's commit messages, for the case the split genuinely cannot be made — an
ABI item the branch renames or removes, whose old form the rest of the tree
still uses. It was a flag on `--land`, whose only record was the commit that
command wrote; a trailer is in the branch's own history, lands with it, and is
the one form CI can read at all, since CI has no command line from the author.
Where the sysroot commits are the *oldest* on the branch the refusal prints the
two-command remedy; where they are interleaved it says so, because nothing in
this workflow rebases. A branch's own update merges are not unrelated work —
merges are excluded, or a branch whose only commit is the ABI change would be
refused for having merged main once. Gates:
`a_branch_mixing_the_sysroot_with_dependent_work_is_refused`,
`the_inseparable_trailer_is_the_escape_and_it_is_in_the_history`,
`an_abi_only_branch_and_an_ordinary_branch_both_pass`.

**A checkout that is merely *behind* main is not diverged from it**, and until
2026-08-07 `standing()` could not tell the two apart: it asked `git diff main`,
which is symmetric, so a worktree that had not merged somebody else's landed ABI
change read as `Diverged` and could claim — rebuilding the shared sysroot from
sources *older* than main's and refusing the checkout whose change had already
landed. It asks `git diff main...HEAD` against the merge base now, plus
`git status` for the working tree, which also catches an untracked file in
`toyos-abi/src` that no diff against a commit could see. Gate:
`a_checkout_behind_main_has_no_standing_to_claim`.

### 3.3 A std edit is made in the primary's `rust/`, and it needs no claim

**Where it has to live.** A linked worktree's `rust/` is the stub §2 leaves and
`toolchain::rust_dir` sends every read to the primary's, so *the*
`rust/library` on this host is one directory and editing the std fork edits it
for every checkout at once. There is nowhere else to put the edit and no way to
hold it privately.

**What that does and does not cost.** It does not cost a claim.
`build/toyos-std-fork-witness` is a hash of `rust/library` kept apart from the
other three witnessed trees, and a worktree finding only *that* stale rebuilds
std rather than being refused (`src/toolchain.rs`, `std_fork_stale`) — because a
rebuild out of the shared `rust/` produces the sysroot every checkout wants and
takes nothing from any of them, where a `toyos-abi` claim refuses all of them
until it lands. Observed 2026-08-11 landing the task #114 std fixes from
`wt/toyos-std`: `The std fork under … has changed since the sysroot was built.
Rebuilding std.`, one rebuild, no claim, no other worktree refused. What it does
cost is that the next build in *every* worktree links against an edit that has
not landed yet, so a std change is landed promptly for the same reason an ABI
change is.

Before 2026-08-10 the witness did not cover `rust/library` at all: the worktree
compiled against the std already on disk and the symptom was the compiler
refusing a method the source plainly has. A brief written before that date says
a std edit needs `--claim-sysroot`. It does not.

**Type-checking one without touching the sysroot at all** is still worth having
while iterating, because the rebuild above is minutes and this is seconds.
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
sees anything. Measured 2026-08-08: 15 s cold, 2.5 s per std edit after.

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
never rewrites history. **It happens on GitHub.**

`cargo run -- --land` used to be the whole protocol: an integration lock on this
host, `git merge --no-ff main`, the whole suite as a gate, `git -C <primary>
merge --ff-only`. It is retired, and the reason is the gate rather than the
mechanism — the dev host is arm64 emulating x86, `specs/ci-plan.md` §7 is a
class of defect it cannot execute at all, and a gate blind to a class is not a
gate against it. The gate is twelve KVM shards on x86_64 now, and the merge went
with it.

In the worktree, at task end:

1. `cargo run -- --pr`. It refuses what would be wrong (on `main`, detached, an
   unresolved merge, an uncommitted tree, a sysroot change carrying dependent
   work), fetches, fast-forwards this host's `main`, merges `origin/main` into
   the branch, and pushes the branch.
2. `gh pr create --fill` the first time; every later `--pr` updates the same
   pull request by pushing to its head.
3. `gh pr merge --auto --merge`, and walk away. GitHub merges when the required
   checks pass.
4. `cargo run -- --sync` afterwards, or at the top of the next task, to bring
   this host's `main` up to what was merged.

**Step 1's merge is the load-bearing one and it is not a convenience.** The
required checks are *strict* — GitHub refuses the merge button until the branch
contains `origin/main` — so the checks that run on the branch head are checks on
the merged result. That is what catches a semantic conflict between two branches
that each pass alone, and it is the one property a naive "CI on the pull request"
setup throws away. GitHub's native merge queue is the feature that restores it
properly; it is **not available on this repository** and `specs/ci-plan.md` §10.1
has the two API answers that say so.

**Serialization comes from the same place.** The first merge moves `main`, and
from that instant every other open pull request is out of date and has to merge
again and re-run. That is the integration lock, enforced by the thing that
actually moves `main` — where the old one was an advisory `flock` on one laptop
that two landings got past, both recorded below.

`specs/ci-plan.md` §10.3 is the table of where each of `--land`'s invariants
went, and §10.4 is the two-stage switch and its trigger.

### What `--pr` refuses rather than guesses at

- **A conflict is left in the worktree**, not aborted — the index and the markers
  git has already written are what the agent resolves against, and an abort
  deletes exactly those. Resolve, `git add`, `git commit`, re-run `--pr`; a run
  that finds `MERGE_HEAD` says so instead of merging over it.
- **An uncommitted tree.** CI gates what was pushed, so uncommitted work would be
  gated by nothing and merged by nothing.
- **A branch mixing the shared sysroot's sources with work that depends on
  them** — §3.2's rule, refused here in a second and again by CI's `abi-split`
  check, both out of `pr::abi_lands_alone`. The escape is an
  `Abi-Inseparable: <why>` trailer in a commit message, which lands with the
  branch and stays as the record; it was a flag, and a flag is a thing CI can
  never see.
- **A push that is not a fast-forward.** Nothing here forces one. A pushed branch
  is a hash somebody's CI run may already have cited.

### The merge commit is the record, and it reads from main's side

git's default for a branch merge is `Merge branch 'main' into wt/toyos-x`, the
direction nobody reads: **166 commits on main in the twenty hours to 2026-08-07,
66 of them that one sentence.** `--land` fixed it by composing the message
itself. GitHub composes it now, out of the pull request's title and body
(`merge_commit_title=PR_TITLE`, `merge_commit_message=PR_BODY`), so the title is
what `git log --oneline` shows and `.github/pull_request_template.md` asks for
the rest. **Read the history with `git log --no-merges`** when you want the work
rather than the landings.

Two things `--land` put in that commit and GitHub does not, and where they went:
**which gate ran** is the check run on the head commit, durable and linked from
the merge; **whether the run was clean** is `EXPECTED_FAILURES` in
`tests/toyos.rs`, which is in the diff being reviewed, and every shard's own
`test result:` line in the run's job summary.

### What is left of the integration lock

`buildlock::integration` survives with a narrower job: **one process at a time
moves this host's `main`**. `--sync` fast-forwards the primary checkout, and the
primary is a checkout somebody may be building in.

The lock's own history is worth keeping, because it is about locks and not about
landing. macOS ships no `flock` CLI, so nothing outside this build system can
take one at all: before `--land` existed, one agent reached for Python and
another put three commits on main during a 10-minute gate. `--land`'s own first
run caught six more (`a5c26b6..8e3f76d`) going onto main by hand, because on main
the command did not exist yet. Both were caught by `--ff-only` doing the work of
a lock nobody was holding — which is why the replacement is enforced by GitHub
rather than by agreement.

**A queue has to be audible**, and on 2026-08-07 eight `--land` processes on this
lock each printed one `[build-lock] waiting …` line and then said nothing for as
long as the seven ahead of them took, which is what a wedge looks like. Every
blocking acquisition in `src/buildlock.rs` repeats itself every 30 s with the
holder re-read each time. Gate: `a_lasting_wait_keeps_saying_so`.

### Who merges: the agent, at task end

Unchanged, and it is the same argument. The current model's one real virtue is
that agents build on each other's landings immediately, and an orchestrator merge
queue gives that up — branches diverge further the longer they wait, and the
person merging has the least context about the change. The agent has the most,
and `gh pr merge --auto` is the agent saying so and then leaving.

What is new is that the owner's leverage now has an artifact. The review gate
this section used to ask for — someone looking at changes to the files that
govern other agents, CLAUDE.md and the specs others read — is a pull request,
which is the first thing this workflow has ever produced that he can review
before it lands. **Zero approving reviews are required**, deliberately: one human
and several agents means a review requirement is a deadlock on him being awake.

**The primary checkout is still not an agent's workspace.** It owns the
toolchain, holds `main`, and `--sync` moves its tree.

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
the SoundFont a worktree needs is `assets/soundfont.sf2` and git carries it, so a
checkout has it like every other asset. There is nothing left to copy —
`specs/ci-plan.md` §6 is the account, and its own SoundFont paragraph predates
this.
