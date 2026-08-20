---
status: open
kind: defect
opened: 2026-08-19
---

# The panel once took no pixels for two minutes under load

First sighting, CI, one red against one green isolated re-run in the same job.
`src/redlist.rs` carries the row.

**What the harness printed** (PR #135 run 32303408773, `guest (11)`):

> `FAIL screen_console_clear: the graffiti actuator did not reach the panel:
> 0 of 2073600 pixels are [0, 192, 0] and the 8px strip below the cells is
> not` — at 127 s against a fast-tier test, so the shape is not a wrong pixel
> but a panel that never received the write inside a window two orders above
> its price. `ALONE: GREEN, and it was alone both times.`
> The same red run's `durations` job then refused the 126,762 ms measurement
> against the 10,000 ms fast line — correctly: that number is this stall, not
> the test's price, and it must never be committed as one.

**The family**: the same evening's `screen_console_panic` sighting (its issue
sits beside this one in the tracker) watched a fatal report lose the screen
under load; this watched an ordinary write lose it. Two tests, one shape —
composition under a loaded host delays or drops the panel's update past any
bound the test grants — and whether the loss is the compositor's, the
actuator's, or TCG starvation is exactly what neither sighting establishes.

The diff each rode on could not have caused it: PR #135 changes a tier
declaration and a duration table.
