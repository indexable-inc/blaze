# sonic-rs

- Upstream: https://github.com/cloudwego/sonic-rs, crates.io `sonic-rs` 0.5.10 (sha256
  756586caeeb21126a57c17d1ef2b9386e2613ba784aae0f27104f5e9982335ec).
- Licence: Apache-2.0 (LICENSE), with the licences of the code it adapts in licenses/.
- Left out: Cargo.lock, Cargo.toml.orig, docs/, scripts/, profile/ and the repository's tool
  configs. The dev-dependencies stay in Cargo.toml, unused: the root manifest excludes it from
  the workspace, so its own tests are not built.

Changes, marked `blaze:` in src/parser.rs:

1. `Parser::skip_escaped_chars` checks that the four bytes after `\u` are hex digits. Upstream
   ate five bytes unchecked, so a string skipped with validation (`skip_one(true)`, what
   `deserialize_ignored_any` uses) such as `"\u"83d\ude00"` swallowed its closing quote and an
   invalid body decoded where serde_json refuses it (blaze-ci
   `webhook::tests::sonic_rs_takes_and_refuses_the_payloads_serde_json_did`, queue 01209). A
   surrogate pair is not checked when skipping, as serde_json's `ignore_escape` does not
   either. Report upstream.
2. A checked skip of an object or array (`skip_one(true)`, so every ignored field) is
   `Parser::skip_nested`, a loop over the containers still open, as serde_json's
   `ignore_value`: the first 64 held on the stack (`Open`), so a shallow skip allocates nothing
   (blaze-ci's `a_warm_delivery_allocates_only_its_events_strings`), deeper ones on the heap. Upstream's `skip_object`/`skip_array` called `skip_one` once
   per level with no depth limit, so an ignored value nested deep enough overflowed the
   thread's stack and aborted the process (the same blaze-ci test on its 2 MiB thread, with a
   field nested 100 deep, in round 8; a webhook body is anyone's). serde_json ignores at any
   depth, so this takes and refuses what upstream did, with the same errors at the same places.
   Report upstream.
