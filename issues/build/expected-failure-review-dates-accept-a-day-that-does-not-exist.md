---
status: open
kind: defect
opened: 2026-08-11
---

# `EXPECTED_FAILURES`' review date accepts `2026-02-31`, and civil-date arithmetic is written twice

`Stale::OnThisDate` is the whole anti-rot property of an intermittent expected
failure: the entry reds on that day whether or not the test ran, and
`check_expected_failures` refuses a date that does not parse because "a date
nothing can read is an entry that never expires". `Day::parse`
(`tests/toyos.rs`) bounds the day at `!(1..=31).contains(&d)` and then hands it
to Hinnant's `days_from_civil`, which is total: it maps the overflow forward
rather than refusing it.

So `2026-02-31` parses, is accepted at startup as a well-formed review date, and
means **2026-03-03**. `2026-04-31` means 2026-05-01, `2026-06-31` means
2026-07-01. The entry expires, so nothing is unbounded — it expires on a day
nobody typed, silently, and the message it prints quotes the date the author
wrote. Both live entries are dated `2026-09-06`, which is a real day, so nothing
is wrong today.

Found while writing `src/redlist.rs`, whose rows carry a `measured` date with the
same shape. That module's `Day` (`src/day.rs`) decides the last day from the
month's own length and refuses the rest; `src/docs.rs`'s `is_a_date`, which
gates every `issues/` frontmatter, was a third copy and now calls it.

**The copy in `tests/toyos.rs` is the one left, and it is the one that gates an
exemption.** It is not `crate::day`'s only because `tests/toyos.rs` already
depends on `toyos_build` (`toyos_build::testargs::parse`), so the change is a
`use` and a deletion — but any edit to that file means a full guest suite, which
is why this is filed rather than done. Whoever takes it should also decide
whether `Day` belongs in `toyos_build` at all or in something both the harness
and the build system depend on.
