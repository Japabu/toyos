---
status: open
kind: defect
opened: 2026-08-19
---

# Two threads faulting on one 2 MiB window each fill their own page, and the loser keeps writing to it

`handle_page_fault` (`kernel/src/process.rs`) answers "is this window already
mapped" under the address-space lock at `:1535`, releases that lock at `:1561`,
and installs the page it filled at `:1643`. The `ProcessData` lock it takes at
`:1563` serialises the fill but not the question: two threads of one process can
both pass the `translate` check, then queue on the process lock and install in
turn.

What that costs:

- Two 2 MiB pages are allocated for one window and both are kept alive by
  `data.demand_pages.push` (`:1645`), so nothing is freed under a live mapping
  and this is not a use-after-free.
- The PDE ends up naming the second page. The first thread's CPU still holds a
  translation for the first, and `remap`'s derived invalidation reaches only the
  CPU that did the second write — no shootdown is issued on this path — so the
  loser goes on reading and writing a page nothing else can see. Two threads of
  one process disagree about the contents of one address.
- A file-backed window is read from the device twice and its relocations are
  applied twice.

The likely shape of the fix is to ask the question again under the lock that
installs — re-`translate` at `:1643`, and drop the page just filled if somebody
else got there — which makes the window a wasted fill rather than a divergence.
Whether that leaves a shootdown owed depends on nothing being installed over a
present entry at all, which is the property the re-check would establish.

Found while making invalidation derive from the entry replaced
(`mm/paging.rs`): the second install is exactly the case where `remap`'s prior
entry is present, and the derivation made it visible.
