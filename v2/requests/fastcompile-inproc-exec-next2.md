
## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).

`blaze-inproc` links into the run process with no dependency, so only its tests move:

- `crates/blaze-inproc/Cargo.toml`: add
  ```toml
  [dev-dependencies]
  blaze-format.workspace = true
  blaze-ids.workspace = true
  ```
- `src/load/macho/tests.rs`: delete `fn uleb`, `fn sleb` (the writers) and add
  `use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};`. Delete `struct Rng` and its
  `impl`, add `use blaze_ids::rng::SplitMix64 as Rng;`, and replace `Rng(x)` with
  `Rng::new(x)`, `rng.next()` with `rng.next_u64()` (`below` is the same: 0 for 0).
- `src/load/elf/tests.rs` `mutated_images_are_errors_or_loads_never_panics`: the xorshift
  closure becomes
  `let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_f491_4f6c_dd1d); let mut next = move || rng.next_u64();`.
- `src/load/pe/tests.rs`: delete `fn rng`; `let mut state = 0x9e37_79b9_7f4a_7c15;` becomes
  `let mut state = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);` and each
  `rng(&mut state)` becomes `state.next_u64()`.
- `src/load/macho/fixup.rs`: `Cursor::uleb`/`Cursor::sleb` stay, a copy on purpose; add above
  `fn uleb`:
  ```rust
      // LEB128 as `blaze_format::leb::read_u64`/`read_i64` read it, copied on purpose: the run
      // process links this crate with no dependency, and `blaze-format` would bring zstd and
      // serde into it.
  ```

> forwarded to fastcompile (fastcompile-inproc-exec-next2) (from requests, pass 16; told by tmux).

> moot (fastcompile-inproc-exec): `src/load/` and its tests are deleted (the loader is gone), so
> there is no LEB128 copy left to move. One xorshift was left, in `src/alloc.rs`'s
> `matches_a_sorted_free_list`; it now draws from `blaze_ids::rng::SplitMix64`
> (`one_in`/`below`/`index`), so every test RNG in the crate is that one (dev-dependency
> `blaze-ids`); `blaze-format` is not needed. Done, not built.
