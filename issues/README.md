# Issues

`issues/` at the repository root is the only prose this project maintains.
Everything else that could be silently false — a spec, a plan, an assessment —
was deleted, because prose is the one artifact nothing checks. An issue is
different in kind: it reproduces or it does not, and closing it costs evidence.

One file per issue, `issues/<area>/<slug>.md`. There is no index and no
numbering: **`ls` is the index and the frontmatter is the query.** A number
encodes a position, and every insertion moved one — which is what made a
reference to an issue a reference that rots.

`ls issues/*/` lists everything. To ask a question of the set:

```
rg -l '^status: open' issues/       # every unheld piece of work
rg -l '^status: assigned' issues/   # what somebody is holding
rg -l '^status: owner' issues/      # what is waiting on the owner
rg -c '' issues/audio/              # how much audio owes
```

## Frontmatter

Four fields, all required, no defaults.

| field | values | means |
|---|---|---|
| `status` | `open` | it is work, and nobody is holding it |
| | `assigned` | it is work, and somebody is — the body says who or which task |
| | `expected-red` | a test fails on this today and `EXPECTED_FAILURES` names it |
| | `owner` | it is the owner's to decide, and nobody else may |
| | `none` | nothing is owed |
| `kind` | `defect` | real, reproducible, someone should fix it |
| | `finding` | noticed in passing; may never be worth fixing |
| | `track` | staged work — something to build that nobody has built |
| | `question` | blocked on the owner, and nobody else can decide it |
| | `rejected` | considered and declined, recorded so nobody re-proposes it |
| `opened` | `YYYY-MM-DD` | the first commit whose issue tracker carried this heading. Before 2026-08-08 that is derived from the single file this directory replaced, so a reworded heading dates from the rewording |
| `task` | a number | optional; present only where the issue names one |

**`status` and `kind` are not free of each other.**
`kind` says what the entry is; `status` says what is owed. Two of the kinds
answer that second question by themselves, so they may not contradict it:

| `kind` | `status` must be |
|---|---|
| `defect`, `finding` | `open`, `assigned` or `expected-red` |
| `track` | `open` or `assigned` |
| `question` | `owner` |
| `rejected` | `none` |

That rule is what makes `rg -l '^status: open'` mean *unheld work* rather than
"every file that was not assigned to somebody" — the eleven `question` and
`rejected` files all said `open` before it existed, so the query over-reported
by eleven and nothing could tell.

**`kind: rejected` is not work.** It is here so the next agent does not spend a
day re-deriving an answer the owner already gave. Nothing in a `rejected` file
is owed — and if the body says otherwise, the *kind* is what is wrong. A ruling
that declared a standing failure rather than removing it deferred the work; it
did not decline it, so the entry is a `defect` and stays open.

**`kind: question` is not work either** — not yours. It is owed by the owner,
and an agent that "fixes" one has decided something that was his to decide. But
a file blocked on an *instrument* — a gate, a machine, a measurement — is not a
question. Nobody has to decide it; somebody has to run it.

**`kind: track` is what a plan used to be**, and it is written to the length a
defect is. A `track` says what is to be built, what it is blocked on, and any
constraint a reader would otherwise pay to re-derive — a hardware bound, a
number somebody measured, a design line the owner already drew. It does not
carry a design, a stage table, a rationale or a review history: a design that is
right is written as code, and one that is not yet written is not yet known. A
`track` that has grown past a screen is a plan again, and is cut back.

## Areas

`isolation` · `panic-path` · `kernel` · `audio` · `diagnostics` · `build` ·
`design-debt` · `hardware` · `filesystem` · `boot-media`

That list is closed. An area is a
directory because it makes every cross-reference a path that resolves. Moving
an issue between areas is a `git mv`; the **slug** is its identity — unique
across every area — so `rg <slug>` finds every pointer at it wherever it has
been put.

## Pointing at one

**Name the file, not the directory.** `issues/audio/hda-tone-phase-check.md`
is a claim something can check; `issues/audio/` is a claim that an area
exists, which says nothing about whether the entry you meant is still there.

Never write "the entry above" or "the entry below". Position was what the
numbered document had and what this directory exists to be rid of; a positional
reference inside a file that no longer sits beside its neighbour points at
nothing at all.

## Filing one

Write a new file. Do not touch an existing one you do not own — nine agents
appending to nine different files produce zero conflicts, and that is the whole
reason this is a directory and not a document.

## Closing one

**Delete the file.** Git keeps the story, and the commit message is where
evidence, measurements and what-the-code-used-to-do belong.

Before you delete it, ask what durable rule it carries — an invariant a future
agent could violate again, independent of the bug that revealed it. One line of
that goes to the module header or the doc comment at the site that owns the
subject, stated as what is true there and citing nothing. The story does not go
with it.

## Two area notes, carried over from the file this replaced

**`filesystem`** — `toyos-fat32/` is new (host tests: `cargo test` inside it) and
its kernel adapter is `kernel/src/fat32_adapter.rs`; `boot-media` carries what
that adapter found. Most of what is filed here is not a defect found later but a
residual the crate's own gate identified while it was being written, recorded so
the adapter's author did not have to rediscover it.

**`boot-media`** — `/boot` and `/log` are both `kernel/src/fat32_adapter.rs` over
`toyos-fat32`, mounted from `gpt::boot_volume()` and `gpt::log_volume()`;
`kernel/src/log_file.rs` writes one file per boot to `/log`, named for the wall
clock. Gated by `esp_filesystem`, `kernel_log_file`, `log_backing_read_error`,
`boot_volume_metadata_error`, `log_partition_automount`, `log_partition_identity`
and `wall_clock_file`, plus `toybox_cp_volume`.
