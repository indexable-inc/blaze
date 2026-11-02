# Requests from usercode-safety-crates

## 1. blaze-global (`crates/blaze-global/src/public.rs`, `Secrets::env`): use the value's own bytes

What: in `Secrets::env`, replace `let bytes = trace::os_bytes(&value);` with
`let bytes = value.as_encoded_bytes();` (and drop the `trace` import if nothing else uses it).

Why: `blaze_usercode::trace::os_bytes` now returns `Cow<'_, [u8]>` (MAY BE UNSOUND 3 of the
safety audit, `v2/requests/usercode.md`): on Windows a name is keyed as its UTF-16 units,
little-endian, so `os_from` can read any bytes back without `from_encoded_bytes_unchecked`.
`self.add(name, bytes)` takes `&[u8]`, so the line no longer compiles as written. And a secret is
found in what a program prints, which holds the value as the platform encodes it
(`as_encoded_bytes`: the same bytes as before on Unix, WTF-8 on Windows), not UTF-16, so the key
encoding was the wrong one for this use anyway. No behavior change on Unix.

> forwarded to the coordinator: `crates/blaze-global` is no lane's in crates.md, so the coordinator's; on requests' coordinator list (it doesn't compile against usercode's `os_bytes` until then).
> forwarded to fastcompile (`crates/blaze-global/` is its, running; told by tmux).
> applied by fastcompile-sol-global, 2026-10-29. The `trace` import stays: `public.rs` still uses `trace::ALL_ENV_NAME` and `trace::untraceable_name`.
