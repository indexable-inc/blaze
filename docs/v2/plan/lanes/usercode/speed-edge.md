---
status: plan
summary: Index of speed-link-edge's work on the usercode lane's test queue, fork server, build-script parsing and Windows sandbox, with where the before/after is.
date: 2026-10-29
---
# speed-edge: tests, build scripts and the Windows sandbox

Part of [usercode](../usercode.md). The details, with the before/after per test, per directive
line and per sandboxed run, are in [speed-link-edge-usercode](speed-link-edge-usercode.md).

- **Test queue and fork server**:
  - frames are read at an offset, never drained;
  - the poll sets and output buffers are reused, and a turn's frames go in one `write`;
  - picking a test is O(log n);
  - one `Effect::Send` per server per call.
- **Build-script output and the elider's lexer**: memchr line and comment search, borrowed values
  until their first escape, and reserved keys matched by a `match`. Nothing is formatted unless
  there is an error.
- **Windows sandbox**: read grants are checked without the session lock, and each path costs one
  `metadata` call.
- Still open: sharing traces between the tests of one server needs `trace::assemble` over borrowed
  lines (`v2/requests/usercode.md`). Write capabilities per directory and an in-process launcher
  are proposals in `v2/requests/usercode-sandbox.md` section 6.
