# Issues

One file per issue, `specs/issues/<area>/<slug>.md`. There is no index and no
numbering: **`ls` is the index and the frontmatter is the query.** A number
encodes a position, and every insertion moved one — which is what made a
reference to an issue a reference that rots.

`ls specs/issues/*/` lists everything. To ask a question of the set:

```
rg -l '^kind: question' specs/issues/     # what is waiting on the owner
rg -l '^status: assigned' specs/issues/   # what somebody is holding
rg -c '' specs/issues/audio/              # how much audio owes
```

## Frontmatter

Four fields, all required, no defaults.

| field | values | means |
|---|---|---|
| `status` | `open` | nobody is holding it |
| | `assigned` | somebody is, and the body says who or which task |
| | `expected-red` | a test fails on this today and `EXPECTED_FAILURES` names it |
| `kind` | `defect` | real, reproducible, someone should fix it |
| | `finding` | noticed in passing; may never be worth fixing |
| | `question` | blocked on the owner, and nobody else can decide it |
| | `rejected` | considered and declined, recorded so nobody re-proposes it |
| `opened` | a date | the first commit whose `specs/issues/` or `specs/issues/` carried this heading. Before 2026-08-08 that is derived from the single file this directory replaced, so a reworded heading dates from the rewording |
| `task` | a number | optional; present only where the issue names one |

**`kind: rejected` is not work.** It is here so the next agent does not spend a
day re-deriving an answer the owner already gave. Nothing in a `rejected` file
is owed.

**`kind: question` is not work either** — not yours. It is owed by the owner,
and an agent that "fixes" one has decided something that was his to decide.

## Areas

`isolation` · `panic-path` · `kernel` · `audio` · `diagnostics` · `build` ·
`design-debt` · `hardware` · `filesystem` · `boot-media`

An area is a directory because it makes every cross-reference a path that
resolves. Moving an issue between areas is a `git mv`; the **slug** is its
identity, so `rg <slug>` finds every pointer at it wherever it has been put.

## Filing one

Write a new file. Do not touch an existing one you do not own — nine agents
appending to nine different files produce zero conflicts, and that is the whole
reason this is a directory and not a document.

## Closing one

**Delete the file.** Git keeps the story, and the commit message is where
evidence, measurements and what-the-code-used-to-do belong.

Before you delete it, ask what durable rule it carries — an invariant a future
agent could violate again, independent of the bug that revealed it. One line of
that goes to the spec or the doc comment that owns the subject. The story does
not go with it.

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
