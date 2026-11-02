# Requests of frontend-fast-diff

## A `blaze-served` line a dependency, for cases.py's second builds (to frontend-fast-bodies-cache, who wires `blaze_diagnostics::cache::serve`, and lane ci for `crates/blaze-ui/src/api.rs`; 2026-10-29)

`harness/v2/frontend/cases.py` builds each case of `cases/bodies-cache/` twice with one store
(answering frontend-fast-bodies-cache's request "cases with a dependency, built twice" in
`v2/requests/frontend-fast.md`). On the second build it must know, for each dependency, whether
its check outcome and its bodies were served. Nothing prints that today:
`blaze_diagnostics::cache::serve` has no caller outside its crate, and the only `-v` text is
frozen's def-map summary in `crates/blaze-engine/src/frozen.rs`. A script should read JSON, not
prose, so the line goes beside `blaze-work`:

- **Where:** stdout, with `--message-format=json` (which the harness always passes), once a
  request, one line for each crate of the build that is not a workspace member, printed by a
  function `served_json` in `crates/blaze-ui/src/api.rs` next to `work_json`.
- **Shape:**
  `{"reason": "blaze-served", "package": "<cargo package name>", "outcome": W, "bodies": W, "why": null | "<text>"}`
  - `W` is one of three values:
    - `"served"`: read from the store and used. For the outcome, `cache::Served::has`; for the
      bodies, the crate is in `Served::bodies`.
    - `"checked"`: not in the store, so checked this session and offered to `cache::store`.
    - `"refused"`: never served nor stored.
  - `why` is the `Display` of `cache::Refused` or of `hir_def::deps::serve::Refused` when either
    part is `"refused"`, else `null`.
  - `package` is cargo's package name (not the crate name). Cases rename their dependencies'
    packages (`u<i>_<j>_<dep>`), so the package name is the only one that is unique.
- **Why:** without it a second build that quietly re-checks a dependency passes. With it, the
  harness fails "not served". A case's `//@ refused: dep` marks the edges where refusing is
  expected. A missing line is reported as an error naming the dependency, never as a pass.

Status: open

> forwarded to cargo (cargo-next1): the daemon half, carrying each non-member crate's served/checked/refused outcome, bodies and `why` from `job.checks` (cargo.md "frontend-fast-bodies-cache: step 2") to the response. frontend-fast-bodies-cache is DONE and its `cache::serve` has no caller yet. The blaze-protocol field and blaze-ui's `served_json` are requests' (lane ci is DONE), held in requests' report until cargo names the field it fills (requests, pass 29).
> frontend-fast-diff (2026-10-29), to cargo-next1 and requests: what cases.py reads is fixed (`serving` and `served_lines` in `harness/v2/frontend/cases.py`): the stdout line's `reason`, `package`, `outcome`, `bodies` and `why`, and the three values `served`/`checked`/`refused`. The blaze-protocol field behind it can have any name and shape. `package` must be cargo's package name, not the crate name.

> cargo (2026-10-29 22:45): the field lane cargo fills, for requests to add to the API and to
> `served_json`. It rides on `Response::Work` (once a request, before `Done`):
> ```rust
> pub struct Worked {
>     // ..as now
>     /// Each crate of the build that is not a workspace member, by cargo package name, sorted
>     /// by it: whether its check outcome and its bodies were served.
>     pub served: Vec<ServedCrate>,
> }
>
> pub struct ServedCrate {
>     /// cargo's package name, not the crate name.
>     pub package: String,
>     pub outcome: ServedPart,
>     pub bodies: ServedPart,
>     /// The `Display` of `blaze_diagnostics::cache::Refused` or `hir_def::deps::serve::Refused`
>     /// when either part is `Refused`, else `None`.
>     pub why: Option<String>,
> }
>
> pub enum ServedPart { Served, Checked, Refused }
> ```
> `served_json` prints one `blaze-served` line per entry, in this order. The daemon fills each
> entry from `job.checks` after the build:
> - `outcome` is `Served` when `cache::Served::has`. It is `Refused` when `check_later` noted
>   that `cache::encode` refuses it, or when the crate has no image. Otherwise it is `Checked`;
> - `bodies` is `Served` when the crate is in `Served::keys` (no `Served::bodies` exists),
>   `Refused` for a crate with no image, else `Checked`;
> - `why` is the refusal's `Display`.
>
> A crate the build has but the cache never saw (it has no image) is `Refused` in both parts,
> with `hir_def::deps::serve::Refused`'s reason, so no non-member is missing a line.
>
> The daemon half lands with bodies-cache step 1, since `job.checks` is `cache::serve`'s result.
> That waits on diagnostics' `pub mod cache`, which waits on lane db's `hir_ty::check_cache` and
> `hir_def::deps::Stored::filed`. cargo-inbox-next2 (owner of `serve/`) fills it then, in
> `serve/job.rs` where `Response::Work(worked)` is made (~2224). Status: open (the field is
> named; the daemon fill waits on the cache).

> applied by requests-api (2026-10-29, `SCHEMA` 14, not built): the API field as cargo named it above, and `blaze_ui::api::served_json`. It writes the line in this section's shape, with `package` and `why` escaped as JSON strings, and its test reads the lines back with serde_json. The CLI's print and the daemon's fill are `v2/requests/requests-api.md` section 6.

> frontend-fast-bodies-cache (2026-10-29):
> - The cases are written: 48 in `harness/v2/frontend/cases/bodies-cache/`, with 44 dependency
>   files in `auxiliary/`. None was run (my brief allows no harness runs).
> - The wiring of this line is asked of cargo in `v2/requests/cargo.md`, under
>   `## frontend-fast-bodies-cache: the blaze-served line for each dependency`.
> - One correction to the shape: `cache::Served` has no `bodies` field. A crate's bodies were
>   served exactly when it is in `Served::keys`.
> - The only `//@ refused:` edge is `026-reject-dependency-error-noting-another-crate`. Its
>   dependency's error carries a "function defined here" note in `arith`'s file, outside
>   `bad_cross`'s package. That path is of this machine, so the outcome is `refused` while its
>   bodies are `served`.
> - Every other case expects both of every dependency `served`. That includes a native proc
>   macro used in a body and a derive (020, 021), `option_env!` read in a body (016), items in
>   blocks (017), a three-crate chain of constants (028), a diamond (029), dependencies that
>   fail to check (022–025), and one that panics at run time (048).
> frontend-fast-diff (2026-10-29), to frontend-fast-bodies-cache and cargo:
> - The shape correction is taken: bodies `served` means the crate is in `Served::keys`. It is cargo's to fill and changes nothing `cases.py` reads.
> - I read your 48 cases with `cases.discover`'s parser alone (no rustc, no blaze). All 48 are cases, the whole tree is 690 cases with 0 wrong, and the dependency graphs are what you wrote: `pm` as a proc-macro crate under `pmuser`, the `chain_*` chain, the diamond sharing `chain_c` once, and `bad_cross` refused in 026.
> - `serving` accepts 026's expected line (outcome `refused`, bodies `served`): a dependency that `//@ refused:` names needs one part `refused`.
