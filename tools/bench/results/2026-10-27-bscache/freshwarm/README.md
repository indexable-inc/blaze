# fresh-none, build scripts as on main and in their closure

`harness/freshwarm.py <project> --edits none`: a seed build pushes its store to a directory
remote; a fresh home and store at the same paths pull it and build again, as a CI runner with
nothing but the checkout and the remote does. ix VM `bscache-fw2`, arm64, 16 cpus, 32 GB
committed, `CARGO_BUILD_JOBS=8`, blaze of this branch (43671a2aac, origin/main 0ef4868a15
merged), the rustup toolchain nightly-2026-10-25 first on `PATH`.

- before: build scripts as on main (`BLAZE_SCRIPT_VIEW` unset).
- after: `BLAZE_SCRIPT_VIEW=on`, the closure taken from `BLAZE_REMOTE` (project
  `nix-closure`, pushed by `blaze-engine closure fetch` + `blaze-engine cache push` once: 37.9 s, 0.58 s).
  `BLAZE_HOME` is one per project for both steps, so the fresh step finds the closure laid out
  (`blaze-engine closure materialize` from the remote into an empty home: 36 s for all 203 paths).

| project | arm | seed build s | fresh pull s | fresh build s | fresh store MB | right |
|---|---|---|---|---|---|---|
| ripgrep | before | 34.16 | 3.57 | 6.67 | 279.4 | yes |
| ripgrep | after | 62.56 | 8.24 | 8.28 | 927.3 | yes |
| nushell | before | 178.82 | 18.32 | 94.98 | 2106.8 | yes |
| nushell | after | 178.15 | 26.00 | 94.19 | 2884.8 | yes |

- Script runs of the fresh step from the store (`blaze: a run the store kept` in each run's
  stderr under `target/rust-analyzer/compile-time/build`): ripgrep 7 of 7 in both arms; nushell
  49 of 50 before, 50 of 50 after. The one before ran is `nu`'s own, which main keys by what it
  was seen reading; in its closure it is keyed by what it is made of.
- After costs the fresh machine its pull: the seed's store holds the closure's packs it pulled
  to lay the closure out, its push carries them, and the fresh pull fetches them (ripgrep 776 MB
  received against 96 MB). Not yet built: a build's push leaving out what `nix-closure` has, and
  laying the closure out only for a run that is not a hit (a hit needs the closure's NarHashes,
  not its files).
- ripgrep's seed is slower after (load 45.0 s against 15.7 s): its first scripts take the closure
  from the remote and lay it out, one at a time (`taking-nix-closure` beside the store).
- Before this branch's last fix, the after arm failed: the scripts of one build pulled the
  closure into one store at once, and a pack another process was fetching was taken for one the
  remote lacked ("blaze's store has no copy of /nix/store/...-libidn2-2.3.8").
- In `nix develop .#bench` rustc is rust-overlay's, which has no rustup channel manifest: blaze
  can't answer `rustc -vV` for it and every load fails, on main too. Hence the rustup toolchain.
