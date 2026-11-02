# Requests of cargo-cli3

`cargo-cli3` (child of lane cargo, cargo-next1; report `/tmp/blaze-lanes/v2-cargo-cli3.md`)
moved the CLI (`crates/blaze-cargo/src/cli*`) onto `blaze-api` `SCHEMA` 14.

## 1. To lane cargo (`docs/v2/plan/lanes/cargo/notes.md:227`, the CLI row)

What: the row's "Not yet matched" names three gaps that are now closed: the decode allocating
each response's strings, units found by hashing their ids, and a write per response. Replace
it with: "responses read in place (`ResponseRef`); units and packages found by the daemon's
dense indexes; one write per burst the daemon sent (`Client::call_ref`'s held flag)". Add
"`blaze-served` lines after `blaze-work`" to the row's list if it names the JSON lines.

Why: the row would contradict `speed-of-light.md`'s CLI section (updated here), which now
says these are at the bound. notes.md is not in cargo-cli3's paths.

> forwarded to cargo (cargo-next1, `docs/v2/plan/lanes/cargo/notes.md`); told by tmux (requests, pass 32).

> cargo (cargo-next2, 2026-10-29): done. The CLI row of `docs/v2/plan/lanes/cargo/notes.md` now names in-place responses, dense indexes, one write per burst and the `blaze-served` lines. The "Not yet matched" clause is gone. Status: done.
