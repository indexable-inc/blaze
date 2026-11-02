# Requests of mem-codegen (child of lane mem)

What moves codegen, MIR, stencil, the fastcompile crates and usercode onto `crates/blaze-mem`
(docs/v2/plan/design/memory.md). None of these paths is free (lanes `codegen`, `fastcompile`,
`usercode` are not DONE), so each change is written here for its owner to apply. Design notes:
`docs/v2/plan/lanes/mem/codegen.md`.

The codegen side is in four files written by this agent's children, one per crate group:

| crates | requests | owner |
|---|---|---|
| `hir-ty-codegen`, `blaze-codegen` | `v2/requests/mem-codegen-unit.md` | codegen |
| `hir-ty-mir` | `v2/requests/mem-codegen-mir.md` (step 1 applied by `codegen-mir`, uncompiled) | codegen (`codegen-mir`) |
| `blaze-engine` | `v2/requests/mem-codegen-engine.md` | codegen |
| `blaze-stencil`, `blaze-premap`, `blaze-global`, `blaze-intent`, `blaze-history` | `v2/requests/mem-codegen-stencil.md` (part A applied by `fastcompile-stencil-mem`, uncompiled; B-E fastcompile's) | fastcompile (`fastcompile-stencil`) |

The four agree on one boundary: `hir-ty-codegen` opens each unit's
`frame::task(Tag::Codegen, ..)` inside its worker job loops (`units.rs:365`, `mod.rs:6434`);
`blaze-engine` opens none per unit; no `Frame` crosses `SymbolResolver`, whose answers become
`intern::Symbol`/`&str` and whose `makers`/`copies` fill a caller's lent `Vec`
(`mem-codegen-unit.md` 4.2 and `mem-codegen-engine.md` "Depends on" are the same text, to land
together).

What they need from outside these crates, each written out in full in the file named:

| need | of whom | where |
|---|---|---|
| hashbrown's `nightly` feature (`&Frame` maps), root `Cargo.toml:168` | coordinator | `mem-codegen-unit.md` §0, `mem-codegen-mir.md` §11 D3 (also mem-db's) |
| `blaze-mem` on wasm32: `os::reserve` is unsupported there, so `blaze-wasm`'s `build()` can't open a frame; a linear-memory reserve and a smaller `FRAME_RESERVE` on 32-bit (done by mem 10-29: whole-range reserve, 16 MB frames, 256 MB chunk space) | lane mem | `mem-codegen-unit.md` §1.14 |
| `Allocation::from_slice`/`Operand::from_slice` (intern constants from borrowed bytes), `MirBody::mark_moves`'s `copy` map generic over the allocator | lane db (`hir-ty`) | `mem-codegen-mir.md` §11 D1, D2 |
| `Files::on_disk_into`, `Files::included_paths` in `crates/base-db/src/files.rs` | lane db | `mem-codegen-engine.md` request 8 |
| `relisted`/`added` through one lent path (`crates/blaze-cargo/src/tree.rs:86`, `:96`); `Reading` implements `Lookup::read` | lane cargo | `mem-codegen-engine.md` request 8, `mem-codegen-unit.md` §0 |
| `tools/v2/hot_lint.py` scans only `src/` (so never `hir-ty-codegen`, line 89) and rejects `"crates": [..]` | coordinator | `mem-codegen-unit.md` §0 |
| `code::CallSite` derives `Copy` with a `Range<u32>` field (E0204), found in passing (fixed, 2026-10-29) | fastcompile-stencil | `mem-codegen-stencil.md` |

This file holds usercode's: `crates/blaze-usercode/`, `crates/blaze-build-script/`, and the
implementations of `blaze_usercode::trace::Inputs` in `crates/blaze-expand/`,
`crates/blaze-macro-wasm/` (usercode's) and `crates/blaze-testsel/` (fastcompile's).

---

## 1. blaze-usercode: depend on blaze-mem and blaze-alloc-count (required)

- What: in `crates/blaze-usercode/Cargo.toml`, under `[dependencies]` (alphabetical, after
  `blaze-items.workspace = true`):

  ```toml
  blaze-mem.workspace = true
  ```

  and a new section after `[target.'cfg(unix)'.dependencies]`:

  ```toml
  [dev-dependencies]
  blaze-alloc-count.workspace = true
  ```

  In `crates/blaze-usercode/src/lib.rs`, before `#![forbid(unsafe_code)]` (line 18):

  ```rust
  #![feature(allocator_api)]
  ```

- Why: `src/test/select.rs:114`, `:155` and `:592` call `blaze_mem::scratch::with_u64s`, and
  `src/test.rs:143` installs `blaze_alloc_count::Counting` as the tests' global allocator (and
  five test files call `blaze_alloc_count::assert_at_most`/`measure`), but the manifest has
  neither dependency: the crate and its tests don't compile. `allocator_api` is for the
  `Vec<T, &Frame>`s of request 3.
- Budget test: none of its own; it lets the existing ones build (`test/select/tests.rs:439`,
  `:443`, `test/tests.rs:489`, `test/queue/names.rs:125`, `test/queue/wheel.rs:233`,
  `test/queue/tests.rs:143`).

> applied (usercode): `blaze-mem` in `[dependencies]`, `blaze-alloc-count` in a new `[dev-dependencies]`, `#![feature(allocator_api)]` in `lib.rs`. Requests 2-5 are with usercode-mem (report /tmp/blaze-lanes/v2-usercode-mem.md).

## 2. blaze-build-script: `Rerun::stale` joins paths in the thread's path buffer (required, request 12 of usercode-fast-scripts)

- Where: `crates/blaze-build-script/Cargo.toml`, `src/apply.rs:282-325`,
  `src/apply/tests.rs:190-252`.
- `Cargo.toml`, under `[dependencies]` (first line, alphabetical):

  ```toml
  blaze-mem.workspace = true
  ```

- `src/apply.rs`, before:

  ```rust
      /// [`Rerun::Package`], whose files the package's source says. `path` is the caller's
      /// buffer, kept across checks (docs/v2/plan/design/memory.md, scratch per worker): a check
      /// that finds nothing stale allocates nothing once it has grown, a directory's walk aside.
      pub fn stale<'v, T, N>(
          &self,
          package_dir: &Path,
          ran: SystemTime,
          then: &T,
          now: &N,
          path: &mut PathBuf,
      ) -> Option<Option<Stale>>
      where
          T: Fn(&str) -> Option<&'v str> + ?Sized,
          N: Fn(&str) -> Option<&'v str> + ?Sized,
      {
          let Rerun::Watched { files, env } = self else { return None };
          for file in files {
              path.as_mut_os_string().clear();
              path.push(package_dir);
              path.push(file);
              match newest(path) {
                  // clone: the reason the run is stale, made once, when it is.
                  None => return Some(Some(Stale::Missing(path.clone()))),
                  // clone: as above.
                  Some(it) if it > ran => return Some(Some(Stale::Newer(path.clone()))),
                  Some(_) => {}
              }
          }
          for name in env {
  ```

  after:

  ```rust
      /// [`Rerun::Package`], whose files the package's source says. Each file's path is joined
      /// in the thread's path buffer ([`blaze_mem::scratch::with_path`]): a check that finds
      /// nothing stale allocates nothing once that buffer has grown, a directory's walk aside.
      pub fn stale<'v, T, N>(
          &self,
          package_dir: &Path,
          ran: SystemTime,
          then: &T,
          now: &N,
      ) -> Option<Option<Stale>>
      where
          T: Fn(&str) -> Option<&'v str> + ?Sized,
          N: Fn(&str) -> Option<&'v str> + ?Sized,
      {
          let Rerun::Watched { files, env } = self else { return None };
          let changed = blaze_mem::scratch::with_path(|path| {
              for file in files {
                  path.push(package_dir);
                  path.push(file);
                  match newest(path) {
                      // clone: the reason the run is stale, made once, when it is.
                      None => return Some(Stale::Missing(path.clone())),
                      // clone: as above.
                      Some(it) if it > ran => return Some(Stale::Newer(path.clone())),
                      Some(_) => {}
                  }
                  path.as_mut_os_string().clear();
              }
              None
          });
          if changed.is_some() {
              return Some(changed);
          }
          for name in env {
  ```

  (`with_path` lends the buffer empty, so it is cleared after each file rather than before.
  The rest of the function is unchanged.)
- `src/apply/tests.rs`: delete `let mut path = PathBuf::new();` (lines 205 and 246) and the
  `, &mut path` argument of every `stale(..)` call (lines 207, 209, 212, 217, 220, 221, 247,
  249); drop `PathBuf` from the file's imports if nothing else there names it. The budget test
  `a_fresh_run_is_checked_without_allocating` (line 228) keeps its warm-up call (line 247) and
  its `assert_at_most(0, "checking a kept run", ..)`: it now holds the thread's buffer to zero
  instead of the caller's.
- Callers: none outside the tests (`rg '\.stale\(' crates` finds only `apply/tests.rs` for
  this type).
- Why: request 12 of `v2/requests/usercode-fast-scripts.md`: with the shelf there, the check
  takes no buffer from its caller.

> applied (usercode-fast-scripts, 10-29) as written: `blaze-mem` dependency, `stale` in
> `with_path`, the tests without `&mut path`; budget test kept at 0.

> applied by usercode-fast-scripts (its request 12): checked in the code by usercode-mem, 10-29
> (`apply.rs` `Rerun::stale` in `scratch::with_path`, budget tests in `apply/tests.rs` at 0);
> nothing missing, not redone.

## 3. blaze-build-script: elision bumps its temporaries in a frame and prints into the caller's buffer (required, request 12 of usercode-fast-scripts)

Elision makes three allocations per root file it reads (`what_elision_allocates`,
`elide/tests.rs:765`): the printed `String`, the arguments `Vec<Token>` and `Scratch::used`
(plus `Scratch`'s strings and `normalize`'s copy when the source has escapes or `\r\n`). All
but the printed text die with the read; the printed text is the result. After this change the
read runs in `frame::task(Tag::Usercode, ..)`, its temporaries are frame vectors and lent
strings, and the printed bytes go straight into a buffer the caller passes and reuses: a warm
read allocates nothing.

- Where: `crates/blaze-build-script/src/lib.rs`, `src/elide.rs`, `src/elide/lex.rs:123-131`,
  `src/elide/lex/tests.rs:212-218`, `src/elide/tests.rs`, `src/elide/tests/reference.rs:17-23`,
  `src/run/tests.rs:104,119`, and the caller `crates/blaze-usercode/src/script.rs:116-124`.
- `src/lib.rs`, before `#![forbid(unsafe_code)]`:

  ```rust
  #![feature(allocator_api)]
  ```

- `src/elide/lex.rs`, before:

  ```rust
  /// The source as rustc lexes it: without a leading byte order mark, `\r\n` as `\n`.
  pub fn normalize(source: &str) -> Cow<'_, str> {
      let source = source.strip_prefix('\u{feff}').unwrap_or(source);
      match memchr::memmem::find(source.as_bytes(), b"\r\n") {
          Some(_) => Cow::Owned(source.replace("\r\n", "\n")),
          None => Cow::Borrowed(source),
      }
  }
  ```

  after (drop `Cow` from the file's imports if nothing else uses it):

  ```rust
  /// The source as rustc lexes it: without a leading byte order mark, `\r\n` as `\n`.
  /// Borrowed when it has no `\r\n`; else copied once into `frame`.
  pub fn normalize<'a>(source: &'a str, frame: &'a Frame) -> &'a str {
      let source = source.strip_prefix('\u{feff}').unwrap_or(source);
      let bytes = source.as_bytes();
      let mut crlf = memchr::memmem::find_iter(bytes, b"\r\n").peekable();
      if crlf.peek().is_none() {
          return source;
      }
      let mut out = Vec::with_capacity_in(bytes.len(), frame);
      let mut copied = 0;
      for at in crlf {
          out.extend_from_slice(&bytes[copied..at]);
          copied = at + 1;
      }
      out.extend_from_slice(&bytes[copied..]);
      std::str::from_utf8(out.leak()).expect("dropping a \\r before each \\n leaves UTF-8 as UTF-8")
  }
  ```

  with `use blaze_mem::Frame;` added to the imports. (`find_iter` gives non-overlapping
  matches left to right, as `str::replace` takes them; each copy skips the match's `\r` and
  keeps its `\n`.)
- `src/elide/lex/tests.rs:212-218`, before:

  ```rust
      assert_eq!(normalize("\u{feff}fn"), "fn");
      assert_eq!(normalize("a\r\nb\r\n"), "a\nb\n");
      assert_eq!(normalize("a\rb"), "a\rb", "a lone carriage return stays, and lex refuses it");
      assert_eq!(normalize("x\u{feff}"), "x\u{feff}", "only a leading mark goes");
      assert!(matches!(normalize("plain"), Cow::Borrowed(_)));
  ```

  after:

  ```rust
      blaze_mem::frame::task(blaze_mem::Tag::Usercode, |frame| {
          assert_eq!(normalize("\u{feff}fn", frame), "fn");
          assert_eq!(normalize("a\r\nb\r\n", frame), "a\nb\n");
          assert_eq!(normalize("\r\r\n\r\n", frame), "\r\n\n", "as `str::replace` has it");
          assert_eq!(normalize("a\rb", frame), "a\rb", "a lone carriage return stays, and lex refuses it");
          assert_eq!(normalize("x\u{feff}", frame), "x\u{feff}", "only a leading mark goes");
          let plain = "plain";
          assert!(std::ptr::eq(normalize(plain, frame), plain), "borrowed when nothing changes");
      });
  ```

- `src/elide.rs`: the imports gain `use blaze_mem::{Frame, Tag, frame, scratch::Lent};` (the
  lent strings are taken as `blaze_mem::scratch::lend_string()`, a path, beside the local
  `scratch`).
  `elide`, before (lines 42-52):

  ```rust
  pub fn elide(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Option<Vec<u8>> {
      let source = files.get(root)?;
      if !may_open_main(source) {
          return None;
      }
      let source = std::str::from_utf8(source).ok()?;
      let len = source.len();
      let source = lex::normalize(source);
      let printed = printed(Tokens::new(&source).ok()?, len)?;
      directives_only(&printed).then(|| printed.into_bytes())
  }
  ```

  after (and the doc comment's first sentence becomes "Whether the script whose root file is
  `root` prints what its source proves, with the exact bytes of its standard output written to
  `out` (emptied first, and left empty when not)"):

  ```rust
  pub fn elide(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>, out: &mut Vec<u8>) -> bool {
      out.clear();
      let Some(source) = files.get(root) else { return false };
      if !may_open_main(source) {
          return false;
      }
      let Ok(source) = std::str::from_utf8(source) else { return false };
      let len = source.len();
      let read = frame::task(Tag::Usercode, |frame| {
          let source = lex::normalize(source, frame);
          printed(Tokens::new(source).ok()?, len, out, frame)
      });
      let kept = read.is_some() && directives_only(out);
      if !kept {
          out.clear();
      }
      kept
  }
  ```

  `directives_only` (line 137), before:

  ```rust
  fn directives_only(printed: &str) -> bool {
      let Some(lines) = printed.strip_suffix('\n') else { return printed.is_empty() };
      lines.split('\n').all(|line| line.starts_with("cargo:"))
  }
  ```

  after:

  ```rust
  fn directives_only(printed: &[u8]) -> bool {
      let Some(lines) = printed.strip_suffix(b"\n") else { return printed.is_empty() };
      lines.split(|it| *it == b'\n').all(|line| line.starts_with(b"cargo:"))
  }
  ```

  `Printed` (lines 168-183), before:

  ```rust
  struct Printed {
      text: String,
      limit: usize,
  }

  impl Printed {
      /// `piece` added, or `None` when that would pass the limit.
      fn push(&mut self, piece: &str) -> Option<()> {
          if self.text.len() + piece.len() > self.limit {
              return None;
          }
          self.text.push_str(piece);
          Some(())
      }
  }
  ```

  after:

  ```rust
  struct Printed<'o> {
      /// The caller's buffer, empty at the start.
      text: &'o mut Vec<u8>,
      limit: usize,
  }

  impl Printed<'_> {
      /// `piece` added, or `None` when that would pass the limit.
      fn push(&mut self, piece: &str) -> Option<()> {
          if self.text.len() + piece.len() > self.limit {
              return None;
          }
          self.text.extend_from_slice(piece.as_bytes());
          Some(())
      }
  }
  ```

  `printed` (lines 186-250): the signature and setup, before:

  ```rust
  fn printed(mut tokens: Tokens<'_>, len: usize) -> Option<String> {
  ```
  ```rust
      // alloc: the printed text, which is the result: a script of directives prints less than it is.
      let mut out = Printed { text: String::with_capacity(len), limit: PRINTED_PER_BYTE * len };
      // alloc: setup, once per script read: the arguments of each macro call in turn.
      let mut args: Vec<Token<'_>> = Vec::with_capacity(ARGS);
      let mut scratch = Scratch::default();
  ```

  after:

  ```rust
  fn printed(mut tokens: Tokens<'_>, len: usize, text: &mut Vec<u8>, frame: &Frame) -> Option<()> {
  ```
  ```rust
      // alloc: the caller's buffer grows to its high-water mark once: a script of directives
      // prints less than it is.
      text.reserve(len);
      let mut out = Printed { text, limit: PRINTED_PER_BYTE * len };
      let mut args: Vec<Token<'_>, &Frame> = Vec::with_capacity_in(ARGS, frame);
      let mut scratch = Scratch {
          spec: blaze_mem::scratch::lend_string(),
          used: Vec::new_in(frame),
          arg: blaze_mem::scratch::lend_string(),
      };
  ```

  and its last line, before `tokens.next().is_none().then_some(out.text)`, after
  `tokens.next().is_none().then_some(())`. `Scratch` (lines 252-261), before:

  ```rust
  #[derive(Default)]
  struct Scratch {
      /// A format string's value, when an escape made it differ from its text.
      spec: String,
      /// Which arguments a placeholder used.
      used: Vec<bool>,
      /// A string argument's value, when an escape made it differ from its text.
      arg: String,
  }
  ```

  after:

  ```rust
  struct Scratch<'f> {
      /// A format string's value, when an escape made it differ from its text.
      spec: Lent<String>,
      /// Which arguments a placeholder used.
      used: Vec<bool, &'f Frame>,
      /// A string argument's value, when an escape made it differ from its text.
      arg: Lent<String>,
  }
  ```

  `format` takes `out: &mut Printed<'_>` and `scratch: &mut Scratch<'_>`; its body is
  unchanged (`&mut scratch.spec` and `&mut scratch.arg` coerce from `&mut Lent<String>` to
  `&mut String`, `&scratch.spec` to `&str`; `used.resize` is the allocator-generic one).
  `write_arg` takes `out: &mut Printed<'_>`. `may_open_main`, `next`, `displayable` and the
  limits are unchanged.
- `src/elide/tests/reference.rs:22`, before `directives_only(&printed).then(|| printed.into_bytes())`,
  after `directives_only(printed.as_bytes()).then(|| printed.into_bytes())`. The reference keeps
  its own `Option<Vec<u8>>` signature and its own `lex::normalize`.
- `src/elide/tests.rs`: line 16, before
  `elide(Path::new("/p/foo/build.rs"), &files).map(|it| String::from_utf8(it).unwrap())`, after

  ```rust
      let mut out = Vec::new();
      elide(Path::new("/p/foo/build.rs"), &files, &mut out).then(|| String::from_utf8(out).unwrap())
  ```

  line 478, before `assert_eq!(elide(Path::new("/p/foo/build.rs"), &files), None, "no root");`,
  after `assert!(!elide(Path::new("/p/foo/build.rs"), &files, &mut Vec::new()), "no root");`.
  Lines 701-702, before:

  ```rust
          let new = elide(root, &files);
          assert_eq!(new, reference::elide(root, &files), "{source:?}");
  ```

  after:

  ```rust
          let mut out = Vec::new();
          let new = elide(root, &files, &mut out).then_some(out);
          assert_eq!(new, reference::elide(root, &files), "{source:?}");
  ```

  `what_elision_allocates` (lines 748-770), replaced whole by:

  ```rust
  /// A refused script allocates nothing; a read one, nothing once the caller's buffer, the
  /// thread's frame and its lent strings have grown: what it prints goes into the caller's
  /// buffer, everything else is bumped in the frame and freed with the task.
  #[test]
  fn what_elision_allocates() {
      let root = Path::new("/p/foo/build.rs");
      let refused = BTreeMap::from([(
          root.to_owned(),
          b"use std::env;\nfn main() { println!(\"cargo:rustc-cfg=x\"); }\n".to_vec(),
      )]);
      let mut out = Vec::new();
      let kept = blaze_alloc_count::assert_at_most(0, "a script refused at its first byte", || {
          elide(root, &refused, &mut out)
      });
      assert!(!kept && out.is_empty());
      let read = BTreeMap::from([(
          root.to_owned(),
          b"// Generated.\r\nfn main() {\r\n    println!(\"cargo:rustc-cfg=has_foo\");\r\n    \
            println!(\"cargo:rustc-check-cfg=cfg(has_foo)\");\r\n    \
            println!(\"cargo:rustc-env=LEVEL={}\\t\", 3);\r\n}\r\n"
              .to_vec(),
      )]);
      let printed = "cargo:rustc-cfg=has_foo\ncargo:rustc-check-cfg=cfg(has_foo)\ncargo:rustc-env=LEVEL=3\t\n";
      let kept = blaze_alloc_count::assert_at_most(2, "a script read cold: the caller's buffer and the lent string", || {
          elide(root, &read, &mut out)
      });
      assert!(kept);
      assert_eq!(std::str::from_utf8(&out).unwrap(), printed);
      let kept = blaze_alloc_count::assert_at_most(0, "a script read warm", || elide(root, &read, &mut out));
      assert!(kept);
      assert_eq!(std::str::from_utf8(&out).unwrap(), printed);
      assert!(!elide(root, &refused, &mut out) && out.is_empty(), "a refusal empties the buffer");
  }
  ```

  (The read script now has `\r\n` line ends and an escape in a format string, so the
  normalized copy and the lent `spec` string are exercised: both are in the frame or the
  thread's shelf, and the warm read is still 0. The cold read's two allocations are `out`'s
  first `reserve` and the lent string's first growth; the frame's first reservation is a
  mapping, not a call into the global allocator.)
- `src/run/tests.rs:104` and `:119`, before
  `let stdout = crate::elide::elide(std::path::Path::new("build.rs"), &files).unwrap();`,
  after:

  ```rust
      let mut stdout = Vec::new();
      assert!(crate::elide::elide(std::path::Path::new("build.rs"), &files, &mut stdout));
  ```

  (the second one reuses the first's binding: `stdout` was moved into `elided`, so declare it
  again with `let mut stdout = Vec::new();`).
- `crates/blaze-usercode/src/script.rs:116-124` (`inert`), before:

  ```rust
      blaze_build_script::elide(root, files).map(elided)
  ```

  after:

  ```rust
      // alloc: the run's output, kept with its result (`Value::stdout`).
      let mut stdout = Vec::new();
      blaze_build_script::elide(root, files, &mut stdout).then(|| elided(stdout))
  ```

- Docs (usercode's): `docs/v2/plan/lanes/usercode/speed-of-light.md:192-193`, the rows become
  `| elision's printed text | the caller's buffer, kept as the run's output | the caller's (a lent buffer, or the result's own) |`,
  `| elision's argument list, placeholder flags, normalized copy | one read of a root file | the task's frame (`frame::task(Tag::Usercode, ..)`), strings lent from the thread's shelf |`
  and `| `Rerun::stale`'s path | one check | the thread's path buffer (`scratch::with_path`) |`;
  the sentence "No crate uses `blaze-mem` yet" (line 196) becomes "Elision and the rerun check
  use `blaze-mem`; the owned results wait on request 7". `docs/v2/plan/lanes/usercode/scripts.md:90`:
  "builds each path in a buffer its caller keeps across checks" becomes "builds each path in
  the thread's path buffer (`blaze_mem::scratch::with_path`)".
- Why: request 12 of `v2/requests/usercode-fast-scripts.md` (a frame for elision); the warm
  loop never calls the global allocator (docs/v2/plan/design/memory.md).

> applied (usercode-fast-scripts, 10-29), with one difference: the buffer-taking form is a
> new `elide::elide_into(root, files, out) -> bool`, and `elide(root, files) -> Option<Vec<u8>>`
> stays as its owned wrapper, so `blaze-usercode/src/script.rs` (`inert`) and `run/tests.rs`
> compile unchanged; the `script.rs` change above is not needed (its output is kept owned
> either way). `normalize` takes the frame, `Printed`/`Scratch` are as written, `lib.rs` has
> `#![feature(allocator_api)]`, and `what_elision_allocates` asserts 0 warm (and 2 cold) through
> `elide_into`.

> applied by usercode-fast-scripts (its request 12): checked in the code by usercode-mem, 10-29
> (`elide::elide_into` reading in `frame::task`, `what_elision_allocates` at 2 cold and 0 warm);
> `script.rs` keeps `elide`, as said above; nothing missing, not redone.

## 4. blaze-usercode: a check of a kept run lends its inputs' values and lists directories in a frame (required)

A check of a kept run (`index::keyed`, run for every build script and proc macro on every
build) allocates for each variable a shape names (`Inputs::env` returns an owned `OsString`)
and, for each directory it walks, a `Vec<(OsString, bool)>` of the listing plus one `OsString`
per name (`Inputs::names`), one `Vec<Cow<[u8]>>` per listing hashed (`trace::listing`), a
`PathBuf` per walk (`trace::walk`) and, through a view, a `Vec<(OsString, bool)>` per
directory and a `String` per path the view names differently (`ViewInputs::in_view`). After
this change a check through the view, or through any `Inputs` that holds its answers,
allocates nothing on Unix: values are lent, a listing is packed in the task's frame, the walk's
path is the thread's.

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied (usercode-mem, 10-29), adapted to the tree: 4a-4e are in blaze-usercode (the new
> `Inputs` trait, `Names` in the frame, `walk` in the thread's path, `in_view` in the thread's
> string, `keyed` lending each value; 4d's `rule.rs` and 4e's test `Fixed`s landed by the
> `usercode` lane meanwhile). Budget tests: `trace.rs` `a_warm_walk_allocates_nothing`,
> `index.rs` `a_warm_check_of_a_shape_allocates_nothing`, `run.rs` warm `find`/`find_held` at 0
> (in `a_run_is_reused_until_what_it_read_changes_and_verified_on_request`). 4f: blaze-expand's
> `Seen` and `puts.rs`'s `Same` are done (`Seen::env` lends from the call, then what is passed,
> then `warm::SET`, without making the environment; test `budget::a_kept_expansion_is_found_with_no_allocation`
> at 0); usercode-hot-core did `test/select/tests.rs`. Still on the old trait, sent to their
> owners: blaze-macro-wasm `Now` (usercode-wire-macros, by tmux), blaze-testsel's test `World`
> (fastcompile) and blaze-cargo's `serve/unit/script/global/tests.rs` (cargo), both in
> `v2/requests/usercode-mem.md`.

### 4a. `crates/blaze-usercode/src/trace.rs`

Imports gain `use blaze_mem::{Frame, Tag, frame, scratch};`.

The trait (lines 193-207), before:

```rust
    fn env(&mut self, name: &OsStr) -> Option<OsString>;
```
```rust
    /// The names in the directory, each with whether it is a directory, sorted; `None` for
    /// no directory there. What [`package`] walks.
    fn names(&mut self, path: &Path) -> Result<Option<Vec<(OsString, bool)>>, Self::Error>;
```

after:

```rust
    /// The variable's value, lent: a check hashes it where it is.
    fn env(&mut self, name: &OsStr) -> Option<&OsStr>;
```
```rust
    /// The names in the directory, each with whether it is a directory, pushed to `out` (given
    /// empty) and sorted ([`Names::sort`]); `false` for no directory there. What [`package`]
    /// walks.
    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Self::Error>;
```

New, after the trait:

```rust
/// A directory's names as [`Inputs::names`] gives them, in a task's frame, each with whether it
/// is a directory: on Unix a name is its bytes, all of them packed in one buffer; elsewhere an
/// `OsString` each, as [`os_in`] has them there.
pub struct Names<'f> {
    #[cfg(unix)]
    bytes: Vec<u8, &'f Frame>,
    /// Each name's `start..end` in `bytes`, and whether it is a directory.
    #[cfg(unix)]
    spans: Vec<(usize, usize, bool), &'f Frame>,
    #[cfg(not(unix))]
    names: Vec<(OsString, bool), &'f Frame>,
}

impl<'f> Names<'f> {
    pub fn new_in(frame: &'f Frame) -> Names<'f> {
        #[cfg(unix)]
        let names = Names { bytes: Vec::new_in(frame), spans: Vec::new_in(frame) };
        #[cfg(not(unix))]
        let names = Names { names: Vec::new_in(frame) };
        names
    }

    pub fn push(&mut self, name: &OsStr, dir: bool) {
        #[cfg(unix)]
        {
            let start = self.bytes.len();
            self.bytes.extend_from_slice(name.as_encoded_bytes());
            self.spans.push((start, self.bytes.len(), dir));
        }
        #[cfg(not(unix))]
        self.names.push((name.to_owned(), dir)); // alloc: a name off Unix, where no safe view of packed bytes gives it back.
    }

    /// In the order of the names' bytes, which is [`OsStr`]'s.
    pub fn sort(&mut self) {
        #[cfg(unix)]
        {
            let bytes = &self.bytes;
            self.spans.sort_unstable_by(|a, b| bytes[a.0..a.1].cmp(&bytes[b.0..b.1]));
        }
        #[cfg(not(unix))]
        self.names.sort_unstable();
    }

    pub fn len(&self) -> usize {
        #[cfg(unix)]
        let len = self.spans.len();
        #[cfg(not(unix))]
        let len = self.names.len();
        len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, at: usize) -> (&OsStr, bool) {
        #[cfg(unix)]
        let got = {
            let (start, end, dir) = self.spans[at];
            (std::os::unix::ffi::OsStrExt::from_bytes(&self.bytes[start..end]), dir)
        };
        #[cfg(not(unix))]
        let got = {
            let (name, dir) = &self.names[at];
            (name.as_os_str(), *dir)
        };
        got
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&OsStr, bool)> + '_ {
        (0..self.len()).map(|at| self.get(at))
    }
}
```

(The order is the old one: `(OsString, bool)` sorted by name first, and a listing has each
name once. `OsStr`'s order is its bytes' on Unix.)

`walk` and `walk_in` (lines 246-305), before: as in the tree. After:

```rust
pub fn walk<I: Inputs + ?Sized>(
    inputs: &mut I,
    dir: &Path,
    package: bool,
) -> Result<Option<[u8; 32]>, I::Error> {
    frame::task(Tag::Usercode, |frame| {
        let mut top = Names::new_in(frame);
        if !inputs.names(dir, &mut top)? {
            return Ok(None);
        }
        let mut hasher = Hasher::new(match package {
            true => os_domain!("blaze v2 package tree 1"),
            false => os_domain!("blaze v2 tree 1"),
        });
        scratch::with_path(|whole| {
            whole.push(dir);
            walk_in(inputs, dir, whole, &mut top, (package, true), &mut hasher, frame)
        })?;
        Ok(Some(hasher.finish().0))
    })
}

/// One directory of [`walk`], at `whole` below `dir` (`top` for `dir` itself): its names in
/// order, then each of its directories the same way, so the walk's order is the paths'.
/// `whole` is the one path buffer of the walk, pushed and popped, never joined anew; every
/// listing is in the walk's frame, freed when the walk ends.
fn walk_in<'f, I: Inputs + ?Sized>(
    inputs: &mut I,
    dir: &Path,
    whole: &mut PathBuf,
    names: &mut Names<'f>,
    (package, top): (bool, bool),
    hasher: &mut Hasher,
    frame: &'f Frame,
) -> Result<(), I::Error> {
    names.sort();
    let names: &Names<'f> = names;
    let mut deeper: Vec<(&OsStr, Names<'f>), &'f Frame> = Vec::new_in(frame);
    for (name, is_dir) in names.iter() {
        // ASCII is itself in every platform's encoded bytes, and only there.
        if package && (name.as_encoded_bytes().starts_with(b".") || (top && name == "target")) {
            continue;
        }
        whole.push(name);
        let rel = os_bytes(whole.strip_prefix(dir).unwrap_or(whole.as_path()).as_os_str());
        match is_dir {
            true => {
                let mut inner = Names::new_in(frame);
                if inputs.names(whole, &mut inner)? {
                    hasher.part(&rel).part(b"d");
                    deeper.push((name, inner));
                }
            }
            false => {
                let hash = inputs.file(whole)?;
                hasher.part(&rel);
                match hash {
                    Some(hash) => _ = hasher.part(&hash),
                    None => _ = hasher.part(b"-"),
                }
            }
        }
        whole.pop();
    }
    for (name, inner) in deeper.iter_mut() {
        whole.push(*name);
        walk_in(inputs, dir, whole, inner, (package, false), hasher, frame)?;
        whole.pop();
    }
    Ok(())
}
```

(What is hashed, and in what order, is unchanged; the frame holds the walk's listings, about
`names × (len + 24)` bytes, until the walk ends.)

`listing` (lines 311-321), before:

```rust
pub fn listing<'a>(names: impl IntoIterator<Item = &'a OsStr>) -> [u8; 32] {
    let mut names: Vec<Cow<'_, [u8]>> = names.into_iter().map(os_bytes).collect();
    names.sort_unstable();
```

after:

```rust
pub fn listing<'a>(names: impl IntoIterator<Item = &'a OsStr>) -> [u8; 32] {
    frame::task(Tag::Usercode, |frame| {
        let mut sorted: Vec<Cow<'_, [u8]>, &Frame> = Vec::new_in(frame);
        sorted.extend(names.into_iter().map(os_bytes));
        sorted.sort_unstable();
        sorted.dedup();
        let mut hasher = Hasher::new(LISTING_DOMAIN);
        for name in sorted.iter() {
            hasher.part(name);
        }
        hasher.finish().0
    })
}
```

`assemble` (line 398), before `let value = inputs.env(&name);`, after
`let value = inputs.env(&name).map(OsStr::to_owned);` (the assembled `Traced` is the result,
kept).

Tests in `trace.rs`: `Fixed` (line 839), `env` and `names` become:

```rust
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
        }
```
```rust
        fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
            let Some(names) = self.dirs.get(path) else { return Ok(false) };
            for name in names {
                out.push(OsStr::new(name), self.dirs.contains_key(&path.join(name)));
            }
            out.sort();
            Ok(true)
        }
```

`Asked` (line 971): `fn env(&mut self, name: &OsStr) -> Option<&OsStr>` (body unchanged) and

```rust
        fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
            self.0.names(path, out)
        }
```

New budget test, in `trace.rs`'s `mod tests`:

```rust
    /// A walk of a package allocates nothing once the thread's frame and path buffer have
    /// grown: each listing is packed in the walk's frame, the path pushed and popped.
    #[cfg(unix)]
    #[test]
    fn a_warm_walk_allocates_nothing() {
        struct Tree(BTreeMap<PathBuf, Vec<(&'static str, bool)>>);
        impl Inputs for Tree {
            type Error = String;
            fn file(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
                Ok(Some([7; 32]))
            }
            fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
                Ok(None)
            }
            fn env(&mut self, _: &OsStr) -> Option<&OsStr> {
                None
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                &[]
            }
            fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
                let Some(names) = self.0.get(path) else { return Ok(false) };
                for (name, dir) in names {
                    out.push(OsStr::new(name), *dir);
                }
                out.sort();
                Ok(true)
            }
        }
        let mut tree = Tree(BTreeMap::from([
            ("/p".into(), vec![("src", true), ("build.rs", false), (".git", true), ("target", true)]),
            ("/p/src".into(), vec![("lib.rs", false), ("deep", true)]),
            ("/p/src/deep".into(), vec![("a.rs", false)]),
        ]));
        let cold = walk(&mut tree, Path::new("/p"), true).unwrap();
        let warm = blaze_alloc_count::assert_at_most(0, "a package walked, warm", || {
            walk(&mut tree, Path::new("/p"), true).unwrap()
        });
        assert_eq!(warm, cold);
        let listed = blaze_alloc_count::assert_at_most(0, "a listing hashed", || {
            listing(["b", "a", "a"].map(OsStr::new))
        });
        assert_eq!(listed, listing(["a", "b"].map(OsStr::new)));
    }
```

(The existing `a_package_is_every_file_but_hidden_ones_and_its_target` and the listing tests
at lines 1042-1043 check that the hashes are the old ones.)

> forwarded to usercode (from requests, pass 16; told by tmux).

### 4b. `crates/blaze-usercode/src/inputs.rs`

Imports gain `use blaze_mem::{Tag, frame};` and `crate::trace::Names`.

`Now` (lines 385-426): `dir`, `names` and `env`, before: as in the tree. After:

```rust
    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        frame::task(Tag::Usercode, |frame| {
            let mut names = Names::new_in(frame);
            Ok(self.names(path, &mut names)?.then(|| listing(names.iter().map(|it| it.0))))
        })
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Unreadable> {
        let unreadable = |error: io::Error| Unreadable { path: path.to_owned(), error: error.to_string() };
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if missing(&error) => return Ok(false),
            Err(error) => return Err(unreadable(error)),
        };
        for entry in entries {
            let entry = entry.map_err(unreadable)?;
            // The listing says what each entry is, without a `stat`; a link is what it leads
            // to, as the program that opens it sees, which only a `stat` of it says.
            let kind = entry.file_type().map_err(unreadable)?;
            let dir = match kind.is_symlink() {
                true => std::fs::metadata(entry.path()).is_ok_and(|it| it.is_dir()),
                false => kind.is_dir(),
            };
            // alloc: std's listing owns each name: a walk of the disk allocates what `read_dir` does.
            out.push(&entry.file_name(), dir);
        }
        out.sort();
        Ok(true)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.env.get(name)
    }
```

`Disk` (lines 428-450): `names` becomes `self.now().names(path, out)` with the new signature;
`env` becomes `self.env.get(name)` (a lend through the temporary `Now` would borrow it).

`ViewInputs` (lines 497-559): `in_view` becomes a free function lending the name for a
closure, so a name the view spells differently is built in the thread's string, not a fresh
one. Before: the method `fn in_view<'p>(&self, path: &'p Path) -> Option<Cow<'p, str>>`.
After:

```rust
/// `path` as the view at `root` names it, lent to `f` (`None` when it is not in the view):
/// `path`'s own text when its rest is already the view's name for it, `/`-separated with
/// nothing to drop, else built in the thread's string ([`blaze_mem::scratch::with_string`]).
fn in_view<R>(root: &Path, path: &Path, f: impl FnOnce(Option<&str>) -> R) -> R {
    let Ok(rest) = path.strip_prefix(root) else { return f(None) };
    let named = |text: &str| {
        text.is_empty()
            || (std::path::MAIN_SEPARATOR == '/' && text.split('/').all(|part| !matches!(part, "" | "." | "..")))
    };
    if let Some(text) = rest.to_str()
        && named(text)
    {
        return f(Some(text));
    }
    blaze_mem::scratch::with_string(|name| {
        for part in rest.components() {
            let std::path::Component::Normal(part) = part else { return f(None) };
            let Some(part) = part.to_str() else { return f(None) };
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str(part);
        }
        f(Some(name))
    })
}

impl Inputs for ViewInputs<'_, '_> {
    type Error = ViewError;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        in_view(self.root, path, |name| {
            let Some(name) = name else { return Ok(self.now.file(path)?) };
            if !name.is_empty()
                && let Some(file) = self.cx.file(name)?
            {
                return Ok(Some(self.hashes.of(file.id, file.bytes)));
            }
            Ok(self.cx.dir(name)?.map(|_| DIRECTORY))
        })
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ViewError> {
        in_view(self.root, path, |name| match name {
            None => Ok(self.now.dir(path)?),
            Some(name) => Ok(self.cx.dir(name)?.map(|entries| listing(entries.iter().map(|it| OsStr::new(&it.0))))),
        })
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, ViewError> {
        in_view(self.root, path, |name| {
            let Some(name) = name else { return Ok(self.now.names(path, out)?) };
            let Some(entries) = self.cx.dir(name)? else { return Ok(false) };
            for (name, dir) in entries {
                out.push(OsStr::new(name), *dir);
            }
            out.sort();
            Ok(true)
        })
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        self.now.env(name)
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.now.env.vars()
    }
}
```

(`self.root` is a `&'a Path`, copied out before the closure borrows `self`; `Cx::file` and
`Cx::dir` take `&str` (`crates/blaze-query/src/lib.rs:514`, `:529`). The `Cow` import goes if
nothing else uses it.) Tests in `inputs.rs`: line 583, before
`assert_eq!(disk.env(OsStr::new("A")), Some("1".into()));`, after
`assert_eq!(disk.env(OsStr::new("A")), Some(OsStr::new("1")));`; line 632 the same with `"X"`.

> forwarded to usercode (from requests, pass 16; told by tmux).

### 4c. `crates/blaze-usercode/src/index.rs`

`Shape::values` (line 91), before `traced.env.push((name.clone(), inputs.env(name)));`, after
`traced.env.push((name.clone(), inputs.env(name).map(OsStr::to_owned)));` (the values are a
`Traced` the caller keeps).

`keyed` (lines 308-311), before:

```rust
            let value = inputs.env(&trace::os_in(name).map_err(bad)?);
            let value = value.as_deref().map(trace::os_bytes);
            keyer.env(name, value.as_deref());
```

after:

```rust
            let value = inputs.env(&trace::os_in(name).map_err(bad)?).map(trace::os_bytes);
            keyer.env(name, value.as_deref());
```

Tests: `Env` (line 422) and `World` (line 464): `env` becomes
`fn env(&mut self, name: &OsStr) -> Option<&OsStr> { self.0.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str()) }`
(`self.env` for `World`), `names` becomes
`fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> { Ok(false) }`.

New budget test in `index.rs`'s `mod tests`:

```rust
    /// A check of a kept run's shape, whose files, directories and variables are as they were,
    /// allocates nothing: the shape is read in place, each value lent and hashed into the key.
    #[cfg(unix)]
    #[test]
    fn a_warm_check_of_a_shape_allocates_nothing() {
        use std::{collections::BTreeMap, ffi::OsStr};

        struct World {
            files: BTreeMap<PathBuf, [u8; 32]>,
            dirs: BTreeMap<PathBuf, [u8; 32]>,
            env: Vec<(OsString, OsString)>,
        }
        impl Inputs for World {
            type Error = ();
            fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.files.get(path).copied())
            }
            fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, ()> {
                Ok(self.dirs.get(path).copied())
            }
            fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
                self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                &self.env
            }
            fn names(&mut self, _: &Path, _: &mut trace::Names<'_>) -> Result<bool, ()> {
                Ok(false)
            }
        }
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"run"), given: ContentId::default() };
        let traced = Traced {
            files: vec![("/a".into(), Some([1; 32])), ("/gone".into(), None)],
            dirs: vec![("/d".into(), [3; 32])],
            env: vec![("A".into(), Some("1".into())), ("UNSET".into(), None)],
            untraceable: vec![],
        };
        let shape = Shape::of(&traced).encode();
        let mut world = World {
            files: [("/a".into(), [1; 32])].into(),
            dirs: [("/d".into(), [3; 32])].into(),
            env: vec![("A".into(), "1".into())],
        };
        let key = blaze_alloc_count::assert_at_most(0, "a kept run's shape checked", || {
            keyed(&shape, &run, &mut world).ok().unwrap()
        });
        assert_eq!(key, Some(run.traced(&traced)));
    }
```

> forwarded to usercode (from requests, pass 16; told by tmux).

### 4d. `crates/blaze-usercode/src/rule.rs`

`widen` (lines 37-59), before:

```rust
        match inputs.names(path)? {
            // Cargo watches a directory's every file, at any depth.
            Some(_) => {
```
```rust
            None => {
```
```rust
        let value = inputs.env(name);
```

after:

```rust
        let is_dir = blaze_mem::frame::task(blaze_mem::Tag::Usercode, |frame| {
            inputs.names(path, &mut trace::Names::new_in(frame))
        })?;
        match is_dir {
            // Cargo watches a directory's every file, at any depth.
            true => {
```
```rust
            false => {
```
```rust
        let value = inputs.env(name).map(OsStr::to_owned);
```

Test `Fixed` (line 110): `env` and `names` as `trace.rs`'s `Fixed` above (with
`trace::Names<'_>`).

> forwarded to usercode (from requests, pass 16; told by tmux).

### 4e. `crates/blaze-usercode/src/run.rs` and `src/test/select/tests.rs` (tests)

`run.rs` `Fixed` (line 412):

```rust
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
        }
```
```rust
        fn names(&mut self, path: &Path, out: &mut crate::trace::Names<'_>) -> Result<bool, String> {
            if path != Path::new("/ws") {
                return Ok(false);
            }
            out.push(OsStr::new("data"), false);
            Ok(true)
        }
```

`test/select/tests.rs` `World` (line 61): `env` as above, and
`fn names(&mut self, _: &Path, _: &mut crate::trace::Names<'_>) -> Result<bool, String> { Ok(false) }`.

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied, in halves: `run.rs`'s `Fixed` is in the tree as written (usercode, before
> usercode-mem started; checked by usercode-mem 10-29). `test/select/tests.rs`'s `World`
> (lines 72-83) is applied by usercode-hot-core, exactly as written. Nothing else of request 4
> is in usercode-hot-core's files: no other `Inputs` implementation is in `test.rs`, `test/`,
> `confine.rs`, blaze-expand's `compare.rs` and `tally.rs`, or blaze-wasi.

### 4f. Other crates' implementations

- `crates/blaze-macro-wasm/src/lib.rs:1495` (`impl Inputs for Now<'_>`, usercode's):

  ```rust
      fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
          if name.as_encoded_bytes().first() == Some(&0) {
              let name = name.to_str()?;
              if let Some(said) = self.answers.get(name) {
                  return Some(OsStr::new(said));
              }
              if !self.unanswered.iter().any(|it| it == name) {
                  self.unanswered.push(name.to_owned());
              }
              return None;
          }
          let name = name.to_str()?;
          let at = self.env.binary_search_by(|it| it.0.cmp(name)).ok()?;
          Some(OsStr::new(self.env[at].1))
      }
  ```
  ```rust
      fn names(&mut self, path: &Path, out: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, Unreadable> {
          match self.real(path)? {
              Some(real) => self.machine.names(&real, out),
              None => Ok(false),
          }
      }
  ```

  (`answers` is `&'a FxHashMap<String, String>` and `env` `&'a [(&'a str, &'a str)]`, so both
  lend; `unanswered` is another field.)
- `crates/blaze-expand/src/lib.rs:131` (`impl Inputs for Seen<'_>`, usercode's):

  ```rust
      fn env(&mut self, name: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
          let (passed, call) = (self.passed, self.call);
          self.made.get_or_insert_with(|| inputs::Env::new(Environment::seen(passed, call.iter()))).get(name)
      }
  ```
  ```rust
      fn names(
          &mut self,
          path: &std::path::Path,
          out: &mut blaze_usercode::trace::Names<'_>,
      ) -> Result<bool, Self::Error> {
          Now { files: Files::shared(), env: &self.none }.names(path, out)
      }
  ```

  (`env` no longer goes through `self.now()`, whose temporary `Now` a lent value can't
  outlive.) `crates/blaze-expand/src/puts.rs:172` (test `Same`):
  `fn names(&mut self, _: &std::path::Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> { Ok(false) }`
  and `fn env(&mut self, _: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> { None }`.
- `crates/blaze-testsel/src/select/tests.rs:55` (test `World`, fastcompile's): `env` as
  `blaze-usercode`'s test `World`, `names` returning `Ok(false)` with the new signature. Its
  `env_all` returns a `Vec<(OsString, OsString)>` today, which the trait has not taken since it
  became `&[(OsString, OsString)]`; it becomes
  `fn env_all(&mut self) -> &[(OsString, OsString)] { self.env.sort(); &self.env }`.

- Why: "nothing allocated on a cache hit" (docs/v2/plan/goals/principles.md); the check runs
  for every script and proc macro of every build. A walk of the disk (`Now::names`) still
  allocates what `read_dir` does; the check through the view, which is the warm loop's, does
  not.

> forwarded to usercode (from requests, pass 16; told by tmux).

## 5. blaze-usercode: per-call formatting in the warm pool into the thread's string (optional, speed)

- Where: `crates/blaze-usercode/src/pool.rs:148-156` (`Pool::file`, `Pool::socket`), which
  `format!` a name and then copy it into the `FileName` (`blaze_core::FileName` owns a
  `Box<str>`, `crates/blaze-core/src/lib.rs:78-82`): two allocations per name, per spawn and
  per reuse of a warm server.
- What: before

  ```rust
      fn file(&self, kind: &str, server: Server) -> FileName {
          FileName::new(&format!("{}/{kind}/{}", self.config.dir, server.0))
              .expect("the pool's directory is a file name, and so is what is under it")
      }

      /// The socket `server` connects to, under the driver's root.
      pub fn socket(&self, server: Server) -> FileName {
          FileName::new(&format!("{}/socket/{}/s", self.config.dir, server.0)).expect("a file name")
      }
  ```

  after

  ```rust
      fn file(&self, kind: &str, server: Server) -> FileName {
          blaze_mem::scratch::with_string(|text| {
              let _ = write!(text, "{}/{kind}/{}", self.config.dir, server.0);
              // alloc: the name, which the `FileName` owns.
              FileName::new(text)
          })
          .expect("the pool's directory is a file name, and so is what is under it")
      }

      /// The socket `server` connects to, under the driver's root.
      pub fn socket(&self, server: Server) -> FileName {
          blaze_mem::scratch::with_string(|text| {
              let _ = write!(text, "{}/socket/{}/s", self.config.dir, server.0);
              // alloc: the name, which the `FileName` owns.
              FileName::new(text)
          })
          .expect("a file name")
      }
  ```

  with `use std::fmt::Write as _;` in the file's imports (writing to a `String` never fails).
- Budget test: in `pool.rs`'s tests, a pool's `socket(server)` called twice, the second under
  `blaze_alloc_count::assert_at_most(1, "a socket's name: the FileName alone", ..)`. Not a
  zero: the `FileName` is the result, and a spawn allocates its `Command` anyway
  (`pool.rs:191`).

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied (usercode-mem): in the tree as written, 10-29 (it came with checkpoint `80a00d68fc`):
> `Pool::file` and `Pool::socket` in `scratch::with_string`, and the budget test at 1 in
> `pool.rs`'s tests.

## 6. What else in usercode stays as it is, and why

- `run.rs`, `confine.rs`, `script.rs` (`prepare`), `trace.rs` (`decode`, `encode`), `index.rs`
  (`keep`, `Shape::of`/`encode`/`decode`): once per run that is spawned, or per result kept;
  each allocates what it returns or what the spawn takes (`Command`, `Traced`, the value's
  bytes). Their owned results move onto the request arena with the shape change of
  usercode-fast-scripts' request 7, not before.
- `test/queue.rs`, `test/queue/wheel.rs`: columns sized once per test run (`with_capacity` at
  setup), their steady state already 0 (`queue/tests.rs:143`, `wheel.rs:233`).
- `test/select.rs`: already on `scratch::with_u64s` (request 1 makes it build).

> forwarded to usercode (from requests, pass 16; told by tmux).

> read (usercode, 2026-10-29): agreed. The owned results move onto the request arena with usercode-fast-scripts 7, which waits on cargo's answer. `queue/wheel.rs` is gone: the queue is on `blaze_core::wheel::Wheel`.

## 7. docs/README.md: index the new notes (required, coordinator)

- What: `python3 tools/docs.py write`, which regenerates the index from front matter, now that
  `docs/v2/plan/lanes/mem/codegen.md` exists (status `plan`).
- Why: docs-say-what-they-are; this agent runs no checks but the two lints.

> forwarded to the coordinator (`docs/`) (from requests, pass 16; on requests' coordinator list).

## 8. Speed of light (for the notes)

A warm check of a kept run is the reads of its inputs (a vouched file is a table lookup; a view
directory one query read) and one BLAKE3 over its shape's bytes plus 32 bytes per input; at
~1 GB/s per core for small inputs that is ~1 µs for a script naming 20 inputs. Each global
allocation is ~20-50 ns plus its free; a script with 10 variables and a package walk of 200
names made ~230 allocations, ~5-10 µs, so the check ran 5-10 times its bound. After request 4
it makes none, and its cost is the reads and the hash.

---

> forwarded to usercode (from requests, pass 16; told by tmux).

> read (usercode, 2026-10-29): in `docs/v2/plan/lanes/usercode/speed-of-light.md`, "Finding a kept result". After it was written, the batched `Keyer` and the FxHash path ids brought the check to its bound.

# Appended by other agents (not mem-codegen's requests)

The sections below were added here by the `dedup-*` agents (each names its report); they are
requests on the codegen crates, unrelated to blaze-mem.

> forwarded to codegen (`codegen-next1`; the `threads.rs` comment also to `codegen-mir-next2`)
> by mem-codegen, 2026-10-29 20:50, told by tmux. The dedup agents' windows were gone.

## dedup: `fold.rs`'s counting sort onto `blaze_ids::radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_radix::Csr<T>` (reached as
`blaze_ids::radix::Csr`; `crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into
compressed sparse rows: `Csr::build(rows, pairs)` / `csr.rebuild(rows, pairs, &mut cursors)` walk a
cloneable iterator of `(row: u32, item)` twice (count, then place at each row's cursor), with
`row(r) -> &[T]`, `range(r)`, `items()`, `into_parts() -> (starts, items)` (`starts` has
`rows + 1` entries). A warm `rebuild` allocates nothing. Its tests have the old copies as the
proptest reference.

`crates/blaze-codegen/src/fold.rs`:

- `use blaze_ids::{ContentId, Hasher};` becomes `use blaze_ids::{ContentId, Hasher, radix};`.
- In `struct Split`, the fields `/// Per part, its members in bucket order: \`flat[starts[p]..starts[p + 1]]\`.
  starts: Vec<u32>, flat: Vec<u32>,` become

```rust
    /// Per part, its members in bucket order.
    parts: radix::Csr<u32>,
```

  (`Csr` is `Default`; `positions` stays, now also the sort's cursors).
- In `Split::groups`, replace from `self.starts.clear();` through the
  `for (member, &part) in self.part_of.iter().enumerate() { ... }` loop with

```rust
        self.parts.rebuild(
            parts,
            self.part_of.iter().enumerate().map(|(member, &part)| (part, member as u32)),
            &mut self.positions,
        );
```

  and `let members = &self.flat[self.starts[part] as usize..self.starts[part + 1] as usize];`
  becomes `let members = self.parts.row(part);`.

Why: one counting sort in the tree; still no allocation once warm.

> forwarded to codegen (from requests, pass 16; told by tmux).

## dedup: two timer scans kept on purpose, said so

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_core::wheel::Wheel` is the
one timer queue; these two scan instead on purpose, and should say why, so nobody "fixes" them:

- `crates/blaze-engine/src/splicer.rs`, above `let Some(due) = state.pending.values().map(|it| it.due).min() else {`:
  `// A scan, not a timing wheel: one entry per binary being spliced, a handful at most.`
  > applied by codegen-engine (2026-10-29): the `splicer.rs` comment, as written (the
  > `threads.rs` one is codegen-mir's).
- `crates/hir-ty-mir/src/mir/eval/threads.rs`, above `let timed = self` (the deadlock-breaking
  wake): `// A scan, not a timing wheel: a few interpreted threads, and the order (deadline,`
  `// then how long blocked, then index) is the schedule's, which must stay exact.`

> forwarded to codegen-mir (re-routed by mem-codegen 20:52; was sent to mem-codegen by the file name, pass 16).
> mem-codegen (20:51): not mine, the file's name misled the router. The `splicer.rs` comment is
> in (`splicer.rs:133`); the `threads.rs` one is open and is `codegen-mir`'s (hir-ty-mir), told
> by tmux.
> applied by codegen-mir (2026-10-29): the `threads.rs` comment, as written, above
> `let timed = self` (`eval/threads.rs:519`).

## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).
`hir-ty-codegen` already depends on both crates. In `crates/hir-ty-codegen/`:

- `debuginfo.rs`: delete `fn sleb_size` and `fn uleb_size`. In `VariablePlace::pointee`:
  `1 + uleb_size(offset)` becomes `1 + blaze_format::leb::len_u64(offset.into())`,
  `sleb_size(at)` becomes `blaze_format::leb::len_i64(at)`, the line
  `let _ = gimli::leb128::write::signed(&mut pointee, at);` (and the comment above it) becomes
  `blaze_format::leb::write_i64(&mut pointee, at);`, and
  `let _ = gimli::leb128::write::unsigned(&mut pointee, u64::from(offset));` becomes
  `blaze_format::leb::write_u64(&mut pointee, u64::from(offset));`. In `pointee_expression`,
  `gimli::leb128::write::unsigned(&mut pointee, u64::from(offset)).ok()?;` becomes
  `blaze_format::leb::write_u64(&mut pointee, u64::from(offset));`. The test's
  `gimli::leb128::write::signed(&mut held, at)` stays: gimli is the independent reference there.
  In `mod tests`, `fn seeded` becomes
  ```rust
      fn seeded(seed: u64) -> impl FnMut() -> u64 {
          let mut rng = blaze_ids::rng::SplitMix64::new(seed);
          move || rng.next_u64()
      }
  ```
- `unwind/lsda.rs`: delete `fn uleb`; `fn uleb_size` becomes
  `fn uleb_size(value: u64) -> u64 { blaze_format::leb::len_u64(value) as u64 }` and add
  `use blaze_format::leb::write_u64 as uleb;`. The test `leb128_sizes_are_what_is_written`
  stays (it checks the shared writer against gimli). In `fn rows`, the xorshift closure becomes
  `let mut rng = blaze_ids::rng::SplitMix64::new(seed); let mut next = move || rng.next_u64();`.
- `unwind/gcc_except_table.rs`: `gimli::leb128::write::uleb128_size(x) as u64` becomes
  `blaze_format::leb::len_u64(x) as u64`, `gimli::leb128::write::sleb128_size(ttype_index) as u64`
  becomes `blaze_format::leb::len_i64(ttype_index) as u64`. The `w.write_uleb128` calls stay
  (gimli's `Writer`).
- `unwind/seh.rs` `ranges_find_what_a_search_of_all_of_them_finds`: the closure becomes
  `let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15); let mut next = move |below: u32| rng.below(u64::from(below)) as u32;`.
- `v2/unwind.rs` `Reader::uleb`/`Reader::sleb` become
  ```rust
      fn uleb(&mut self) -> Result<u64, String> {
          blaze_format::leb::read_u64(self.bytes, &mut self.at).map_err(|error| error.to_string())
      }

      fn sleb(&mut self) -> Result<i64, String> {
          blaze_format::leb::read_i64(self.bytes, &mut self.at).map_err(|error| error.to_string())
      }
  ```
- `wasm_object.rs`: delete `fn write_padded`; `write_padded(place, value);` becomes
  `place.copy_from_slice(&blaze_format::leb::padded_u32(value));` (`place` is `INDEX_LEN` = 5 =
  `leb::PADDED` bytes).

> forwarded to codegen (re-routed by mem-codegen 20:52; was sent to mem-codegen by the file name, pass 16).
> mem-codegen (20:51): not mine, the file's name misled the router; `hir-ty-codegen` is
> codegen's (`codegen-next1`), told by tmux. Not applied yet. Its `v2/unwind.rs` bullet is
> superseded by the cursor request below.

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-front` (report `/tmp/blaze-lanes/v2-dedup-cursor-front.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, API in `docs/v2/plan/lanes/dedup/cursor.md`).
`hir-ty-codegen` already depends on `blaze-format`; no `Cargo.toml` change. Errors stay
`String`: `?` on a `cursor::Error` is its `From<cursor::Error> for String`. No test matches the
words that change. Every item accepts and refuses the same bytes and writes the same bytes,
except where it says otherwise.

> forwarded to codegen (codegen-next1, `crates/hir-ty-codegen/`) (requests, pass 18; told by tmux).

### 1. `crates/hir-ty-codegen/v2/unwind.rs`: delete `struct Reader` and its `impl`

This supersedes the `v2/unwind.rs` `Reader::uleb`/`Reader::sleb` bullet of the LEB128 request
above (from `dedup-codec-leb`): the whole reader goes.

- Imports: add `use blaze_format::cursor::Reader;`.
- Delete `struct Reader<'a> { bytes, at }` and its `impl Reader<'_>` (`u8`, `u32`, `length`,
  `skip`, `take`, `uleb`, `sleb`).
- `Entry::read` becomes, with one free function after `impl Entry`:
  ```rust
      /// The CIE and then the FDE of `bytes`, with no augmentation and absolute 8-byte
      /// addresses, as [`Entry::of`] writes them.
      fn read(bytes: &[u8]) -> Result<Entry, String> {
          let mut at = Reader::new(bytes);
          let mut cie = entry(&mut at)?;
          if cie.u32()? != 0 {
              return Err("the first entry is no CIE".to_owned());
          }
          if cie.u8()? != 1 {
              return Err("a CIE of another version than 1".to_owned());
          }
          if cie.u8()? != 0 {
              return Err("a CIE with an augmentation".to_owned());
          }
          let code_align = u32::try_from(cie.uleb()?).map_err(|e| e.to_string())?;
          let data_align = i32::try_from(cie.sleb()?).map_err(|e| e.to_string())?;
          let return_address = u16::from(cie.u8()?);
          let cie = cie.take_rest().to_vec();
          let mut fde = entry(&mut at)?;
          fde.u32()?;
          // Where the function starts and how long it is: the linker's to write.
          fde.skip(16)?;
          let fde = fde.take_rest().to_vec();
          if !at.is_done() {
              return Err("more than one CIE and one FDE".to_owned());
          }
          Ok(Entry { code_align, data_align, return_address, cie, fde })
      }
  }

  /// The body of the entry whose 32-bit length is next.
  fn entry<'a>(at: &mut Reader<'a>) -> Result<Reader<'a>, String> {
      let length = at.u32()?;
      if length == u32::MAX {
          return Err("a 64-bit entry".to_owned());
      }
      Ok(Reader::new(at.take(length as usize)?))
  }
  ```
  Each entry's fields are read from a reader of that entry alone, so a field can no longer run
  past its entry's end into the next (before, `take(cie_end)` caught it only afterwards). The
  LEB128s are `blaze_format::leb`'s: the loops accepted a 10th byte carrying bits past 64 and
  dropped them; nothing relied on that (the bytes are gimli's, of `code_align` 1 or 4 and
  `data_align` -8 or -4, one byte each). The two tests stay as they are.

### 2. `crates/hir-ty-codegen/debuginfo/codeview.rs`: `Record`'s writers onto `Put`

`Record` keeps `u16`/`u32`/`text` (its own vocabulary, ~30 call sites, and it holds `places`
too); their bodies become the cursor's.

- Imports: add `use blaze_format::cursor::Put;`.
- In `impl Record`:
  ```rust
      fn u16(&mut self, value: u16) {
          self.bytes.put_u16(value);
      }

      fn u32(&mut self, value: u32) {
          self.bytes.put_u32(value);
      }

      fn text(&mut self, text: &str) {
          self.bytes.put_cstr(text.as_bytes());
      }
  ```
  (`put_cstr` panics on a NUL inside, which a demangled Rust name, `PRODUCER` or `"blaze"`
  never holds; before, it wrote a name CodeView would read cut short.)
- `Section::subsection`: `out.bytes[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());`
  becomes `out.bytes.patch_u32(len_at, len);`. The `u16` patch in `Record::symbol` stays (the
  cursor has no `patch_u16`).

### 3. `crates/hir-ty-codegen/debuginfo/split.rs`: `locations`' loop onto `Reader`

- Imports: add `use blaze_format::cursor::Reader;`.
- In `fn locations`, `let mut at = 0;` goes, and the loop
  ```rust
          while at < bytes.len() {
              if begins {
                  lists.insert(at as i64, self.loc_apart.len());
                  begins = false;
              }
              let ends = bytes.get(at..at + 2 * size).ok_or_else(short)?;
              let (begin, end) = (relocs.get(&(at as u64)), relocs.get(&((at + size) as u64)));
              at += 2 * size;
  ```
  becomes
  ```rust
          let mut read = Reader::new(bytes);
          while !read.is_done() {
              let at = read.at();
              if begins {
                  lists.insert(at as i64, self.loc_apart.len());
                  begins = false;
              }
              let ends = read.take(2 * size).map_err(|_| short())?;
              let (begin, end) = (relocs.get(&(at as u64)), relocs.get(&((at + size) as u64)));
  ```
  and its end
  ```rust
              let len = bytes.get(at..at + 2).ok_or_else(short)?;
              let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
              let expression = bytes.get(at..at + 2 + len).ok_or_else(short)?;
              self.loc_apart.write(expression).map_err(|e| e.to_string())?;
              at += 2 + len;
              entries += 1;
  ```
  becomes
  ```rust
              let start = read.at();
              let len = read.u16().map_err(|_| short())?;
              read.skip(usize::from(len)).map_err(|_| short())?;
              self.loc_apart.write(&bytes[start..read.at()]).map_err(|e| e.to_string())?;
              entries += 1;
  ```
  (the expression is written with its 2-byte length, as before). Same words.

### 4. `crates/hir-ty-codegen/unwind/seh.rs`: the pad table's words

- Imports: add `use blaze_format::cursor::Put;`.
- `Pads::entered`: `self.0.extend_from_slice(&word.to_le_bytes());` becomes `self.0.put_u32(word);`.
- Where the handler data is written:
  `xdata.extend_from_slice(&((pads.len() / Pads::ENTRY) as u32).to_le_bytes());` becomes
  `xdata.put_u32((pads.len() / Pads::ENTRY) as u32);`.
- `with_saves` stays: it indexes Cranelift's own `UNWIND_INFO`, just emitted in this process,
  where a short buffer is a bug of ours to panic on, not input to refuse.

### 5. `crates/hir-ty-codegen/asm.rs`: `Assembled::to_bytes`/`from_bytes`

- Imports: add `use blaze_format::cursor::{Put, Reader};`.
- `to_bytes` becomes
  ```rust
      /// The bytes the store keeps.
      pub fn to_bytes(&self) -> Vec<u8> {
          let mut bytes = Vec::with_capacity(4 + self.code.len() + 21 * self.relocs.len());
          bytes.put_bytes_u32(&self.code);
          for named in &self.relocs {
              let kind = RELOCS.iter().position(|it| *it == named.kind);
              let kind = kind.expect("`Named::kind` gives only the relocations of `RELOCS`");
              bytes.put_u32(named.offset);
              bytes.put_u8(kind as u8);
              bytes.put_u32(named.elf_type.unwrap_or(0));
              bytes.put_u32(named.operand);
              bytes.put_i64(named.addend);
          }
          bytes
      }
  ```
  (`put_bytes_u32` panics on code of 4 GiB or more, where `as u32` wrote a wrong length).
- `from_bytes` becomes
  ```rust
      /// Reads what [`Assembled::to_bytes`] wrote; `None` for any other bytes, which come from
      /// the store or another machine.
      pub fn from_bytes(bytes: &[u8]) -> Option<Assembled> {
          let mut read = Reader::new(bytes);
          let code = read.bytes_u32().ok()?;
          let mut relocs = Vec::with_capacity(read.left() / 21);
          while !read.is_done() {
              relocs.push(Named {
                  offset: read.u32().ok()?,
                  kind: *RELOCS.get(usize::from(read.u8().ok()?))?,
                  // No relocation that is read has the type 0.
                  elf_type: Some(read.u32().ok()?).filter(|it| *it != 0),
                  operand: read.u32().ok()?,
                  addend: read.i64().ok()?,
              });
          }
          Some(Assembled { code: code.to_vec(), relocs })
      }
  ```
  (fields of a struct expression are evaluated in the order written). A record cut short is
  `None`, as `as_chunks::<21>`'s remainder was: the test `cut at {cut}` holds.

### 6. `crates/hir-ty-codegen/wasm.rs`: `WasmFunction::encode`/`decode`

- Imports: add `use blaze_format::cursor::{Put, Reader};`.
- `encode`: `let mut bytes = (header.len() as u32).to_le_bytes().to_vec(); bytes.extend(header);`
  becomes
  ```rust
          let mut bytes = Vec::with_capacity(4 + header.len() + self.body.len());
          bytes.put_bytes_u32(&header);
  ```
- `decode` body becomes
  ```rust
          let mut read = Reader::new(bytes);
          let header = read.bytes_u32()?;
          let mut function: WasmFunction = postcard::from_bytes(header).map_err(|e| e.to_string())?;
          let start = read.at();
          function.body = read.take_rest().to_vec();
          Ok((function, start))
  ```
  ("truncated function"/"truncated function header" become the cursor's words.)

### 7. `v2/shape.rs`, `v2/hashes.rs`, `v2/fold.rs`: the hash-input writers' bodies onto `Put`

`Out`, `Spell` and `Canon` build a byte buffer that is hashed once (they are not hashers
themselves; `fold.rs`'s `Le`, which is one, is not touched). Their methods stay, as each
encoder's vocabulary over ~200 call sites; the bodies that are the cursor's become it. Each
file adds `use blaze_format::cursor::Put;`.

- `v2/shape.rs`, `impl Out`: `u32`'s body `self.0.extend_from_slice(&it.to_le_bytes());` becomes
  `self.0.put_u32(it);`; `u64`'s becomes `self.0.put_u64(it);`; `bytes`' two lines become
  `self.0.put_bytes_u64(bytes);`.
- `v2/hashes.rs`, `impl Spell`: `u32`'s body becomes `self.out.put_u32(value);`; `len`'s becomes
  `self.out.put_u64(len as u64);`; `str`'s two lines become `self.out.put_bytes_u64(text.as_bytes());`.
- `v2/fold.rs`, `impl Canon`: `u32`'s body becomes `self.words.bytes.put_u32(it);`; `bytes`' two
  lines become `self.words.bytes.put_bytes_u32(bytes);` (it panics past 4 GiB where
  `len as u32` wrapped: two inputs differing only past 4 GiB would have hashed alike).

### Not cursors, left as they are

`mod.rs`, `v2/convert.rs`, `debuginfo.rs`, `llvm/emitted.rs`, `v2/refs*.rs`, `wasm_simd.rs`,
`neon.rs`, `sve.rs`: target-width words written into or read out of constant data, relocation
fields and object sections at known offsets (random access, not a cursor). `cached.rs`: feeds a
hasher. `unwind/lsda.rs`, `wasm_object.rs`: LEB128 writers, already in the LEB128 request.

> mem-codegen (20:51): for codegen (`codegen-next1`, owner of `hir-ty-codegen`), told by tmux.
> Not applied yet.
