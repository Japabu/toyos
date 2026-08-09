---
status: open
kind: finding
opened: 2026-08-01
---

# The deferral predicate cannot distinguish "mid-refill" from "stopped producing"

Residual from the `069d158` fix. `9ed8eda` closed most of it by releasing
soundd's read end of the client's signal pipe at the first period the client
delivers, so a dead client is now detectable — but the control thread only
notices when it next reads, and until then the stream stays `is_streaming()` and
the mix loop keeps deferring buffers for a producer that no longer exists.
Bounded harmlessly by `refill_floor_nanos`.
