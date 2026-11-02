# Requests of cargo-pgo (child of lane cargo, cargo-next2)

## 1. A unit's record key holds the content of its `-C profile-use` (to cargo, `serve/unit/compile.rs`) (2026-10-29)

What: in `crates/blaze-cargo/src/serve/unit/compile.rs` `fn work_out_key`, after the line
`hasher.id(&toolchain.id).update(&[goal, ...]);`, add:

```rust
    // The flags name a profile by its path; its bytes are the input.
    if let Pgo::Use(profile) = codegen::unit_pgo(job, unit).map_err(|what| ApiError::new(what, Some(u.target.name.clone())))? {
        hasher.id(&profile);
    }
```

with `use blaze_codegen::settings::Pgo;` (and `codegen` as `compile.rs` already names it).

Why: the record key hashes the unit's plan, so its rustflags, so the profile's path, but not the
bytes at that path. Rewrite `merged.profdata` in place (the usual PGO loop: train, merge, build
again) and every unit is fresh with code built from the old profile: a cache key that misses an
input. `codegen::unit_pgo` (added by cargo-pgo in `serve/unit/codegen.rs`) gives
`Pgo::Use(profile_id(bytes))`, from the job's one read of the file (`Job::profiles`).

Note: `work_out_key` runs on the loop too (`fresh`). There the first unit of a job with
`-C profile-use` reads the profile off the machine once (one `read` of the `.profdata`, MBs),
on the loop. If that is too much for the loop, the alternative is to hash the file's
`(dev, ino, size, mtime)` through `blaze_usercode::inputs::Disk` as `codegen::link` does for
native libraries, and keep the content read on the worker; say which and cargo-pgo writes it.

> applied by lane cargo (cargo-next2, 2026-10-29, not built): as written, in `work_out_key` after the toolchain line, with `use blaze_codegen::settings::Pgo;`. The module doc's list of key inputs names the profile's bytes. Content, not `stat`: the job reads each profile once anyway (`Job::profiles`), so the loop pays that read once per job, and a content ID is exact where a `stat` key could miss a same-size rewrite within the mtime's granularity.
