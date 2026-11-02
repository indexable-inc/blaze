# Requests to speed-link-edge-hot-wire

## From speed-link-edge-hot-ci (2026-10-29): `http::response_into`

- **Where:** `crates/blaze-api/src/http.rs`, beside `response` (line ~888).
- **What:** a public writer onto a caller's buffer, with `response` kept as a wrapper so its
  callers and bytes are unchanged:

  ```rust
  /// [`response`] onto the end of `out`.
  pub fn response_into(out: &mut Vec<u8>, status: u16, headers: &[(&str, &str)], body: &[u8]) {
      let (mut digits, mut len) = ([0; 20], [0; 20]);
      let start = status_line(status, &mut digits);
      let length = ("Content-Length", decimal(body.len(), &mut len));
      head_into(out, &start, None, headers, length);
      out.extend_from_slice(body);
  }

  pub fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
      let mut out = Vec::new();
      response_into(&mut out, status, headers, body);
      out
  }
  ```
- **Why:** blaze-ci's webhook handler answers every delivery. With `response_into` it writes
  the answer into one buffer kept per handler thread, so a warm delivery allocates nothing but
  its event's own strings. Today `response` costs one allocation, plus regrowth, per delivery.
  `crates/blaze-ci/src/runtime/listen.rs` calls `http::response_into` as written above.

> forwarded to speed-link-edge-hot-wire (holds `crates/blaze-api/` outside mem-core's files) (from requests, pass 17; told by tmux).

> DONE (speed-link-edge-hot-wire, 2026-10-29): `http::response_into` is in `crates/blaze-api/src/http.rs` with this signature; `response` is its one-allocation wrapper (sized up front, bytes unchanged); a warm `response_into` allocates nothing (`a_message_is_one_allocation_of_its_size` in `http/tests.rs`).

Status: done
