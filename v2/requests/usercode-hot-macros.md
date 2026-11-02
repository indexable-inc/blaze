
## dedyn: crates/blaze-expand/src/native/: the in-process host is a concrete type, `dyn` only for what asks the database (user's call, 2026-10-29)
> Being applied in the tree by dedyn-macro (dedyn, 2026-10-29 21:22, new edit rule): don't apply it again.
> APPLIED by dedyn-macro 2026-10-29 21:34: parts 1-5 as written (the `Host` methods `pub`, so
> `sys` and tests reach them); test `a_span_function_the_bridge_answers_alone_gives_the_wasm_hosts_word`
> in `crates/blaze-macro-wasm/src/lib.rs` (dev-dependency `native-proc-macro`).


Why: `__native::HOST` holds `*mut dyn Host` and every `sys` function, every `word` (one `pack`
per input token, `registered`/`stream`) and every `unpacked` (one `get` per output token) goes
through its vtable; `quote!`/`syn` call `Span::call_site`, `resolved_at` and `located_at` per
token they make. There is one implementation (`native/host.rs`). The `proc_macro` crate can't name
it because blaze-expand depends on it, but everything a per-token call touches is `Parts` and the
three site words, which need nothing of blaze-expand. So: the host becomes a concrete struct
of the `proc_macro` crate owning `Parts` and the sites, and only what needs the database or the
lexer (line/column, file, source text, parent, source, byte range, join, subspan, text to tokens)
crosses a `dyn Ask` that blaze-expand implements. That `dyn` is a real boundary (the crate cycle)
and off the per-token path. No code-size cost: nothing becomes generic. The wasm guest
(`blaze-macro-wasm/guest/proc_macro.rs`) already calls the host through static wasm imports and
needs no change; the wasm host keeps reading `Parts` through `bridge::wire`'s re-export.
dedup-macros (DONE) merged these files; told it.

> forwarded to usercode (owns `crates/blaze-expand/`, lane not DONE; told by tmux; requests, pass 22).
> taken by usercode: a child, `usercode-dedyn-native`, applies parts 1-5 once usercode-hot-macros (holding `native/` for the hot lint) is done.

### 1. `Parts` moves into the `proc_macro` crate

- `crates/blaze-expand/src/native/proc_macro/Cargo.toml` `[dependencies]`: add
  `rustc-hash.workspace = true`.
- Move `crates/blaze-expand/src/bridge/wire.rs` lines 17–193 verbatim (from `/// The word of no
  span` through the end of `impl Parts`, i.e. `NONE`, `WIDE`, `NARROW`, `SCAN`, `Part`, `Parts`
  and its `impl`) into `proc_macro/lib.rs`'s `pub mod __native`, after its `use`s, with these
  three visibility changes, since `wire.rs`'s reader uses them:
  `const WIDE` → `pub const WIDE`, `const NARROW` → `pub const NARROW`,
  `fn part_written` → `pub fn part_written` (each gets `#[doc(hidden)]`). Add to `__native`:
  `use rustc_hash::FxHashMap;` and
  `use span::{EditionedFileId, ErasedFileAstId, SpanAnchor, SyntaxContext, TextRange, TextSize};`
  plus `use tt::WirePart;` (the moved block names them; `span::Span` is already in scope as
  `span::Span` there, so in the moved block write `span::Span` where it says `Span`, because
  `__native`'s `Span` is the macro's).
- In `wire.rs`, in place of the moved lines:

```rust
pub use native_proc_macro::__native::{NARROW, NONE, Part, Parts, WIDE};
```

  and drop the now-unused names from its `use rustc_hash::FxHashMap;` / `use span::{..}` lines
  (`FxHashMap`; keep what `relative`/`read` still use).

> forwarded to usercode (requests, pass 22).
> applied (usercode-dedyn-native): found in the tree as asked (by dedyn-macro), checked
> against the current code: `rustc-hash` in `proc_macro/Cargo.toml`, `NONE`..`impl Parts` in
> `__native` with `pub` `WIDE`/`NARROW`/`part_written`, `wire.rs` re-exports them and no longer
> imports `FxHashMap` or the span names; the column wire and reused `Parts` kept.

### 2. `proc_macro/lib.rs` `__native`: replace `pub trait Host { .. }` (lines 295–327) with

```rust
    /// What only blaze's database or lexer can answer of an expansion: asked when a macro
    /// asks a span's place, file or text, joins, parents or cuts a span, or makes tokens of
    /// text, never per token.
    pub trait Ask {
        /// The span covering both, `None` when they can't be joined.
        fn join(&mut self, first: span::Span, second: span::Span) -> Option<span::Span>;
        fn parent(&mut self, span: span::Span) -> Option<span::Span>;
        fn source(&mut self, span: span::Span) -> Option<span::Span>;
        /// The part of `span` between the module's encoded bounds, `None` when outside it.
        fn subspan(&mut self, span: span::Span, start: u64, end: u64) -> Option<span::Span>;
        fn byte_range(&mut self, span: span::Span) -> [u32; 2];
        fn line_column(&mut self, span: span::Span) -> Option<(u32, u32)>;
        fn file(&mut self, span: span::Span) -> Option<String>;
        fn local_file(&mut self, span: span::Span) -> Option<String>;
        fn source_text(&mut self, span: span::Span) -> Option<String>;
        /// The stream of `text` at `call_site` as the columns [`wire::Reader`] reads, its
        /// spans given words in `parts`, or why it is none.
        fn ts_from_str(&mut self, parts: &mut Parts, text: &str, call_site: span::Span)
            -> Result<Vec<u8>, String>;
        /// The literal of `text` as the columns of a stream of it alone.
        fn literal_from_str(&mut self, parts: &mut Parts, text: &str, call_site: span::Span)
            -> Option<Vec<u8>>;
    }

    /// The host of one expansion: its span parts and the words of its def, call and mixed
    /// sites, answering every span function a macro calls per token itself.
    pub struct Host<'a> {
        pub parts: &'a mut Parts,
        pub sites: [u64; 3],
        // dyn: blaze-expand's database and lexer, which this crate can't name (blaze-expand
        // depends on it); asked only off the per-token path (`Ask`).
        pub ask: &'a mut dyn Ask,
    }

    impl Host<'_> {
        fn span(&self, word: u64) -> Option<span::Span> {
            self.parts.get(word)
        }

        fn call_site_span(&self) -> span::Span {
            self.span(self.sites[1]).expect("the current call site is one of the expansion's")
        }

        fn def_site(&mut self) -> u64 {
            self.sites[0]
        }

        fn call_site(&mut self) -> u64 {
            self.sites[1]
        }

        fn mixed_site(&mut self) -> u64 {
            self.sites[2]
        }

        fn span_join(&mut self, a: u64, b: u64) -> u64 {
            let (Some(first), Some(second)) = (self.span(a), self.span(b)) else { return NONE };
            self.ask.join(first, second).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_resolved_at(&mut self, a: u64, at: u64) -> u64 {
            let (Some(span), Some(at)) = (self.span(a), self.span(at)) else { return a };
            self.parts.pack(span::Span { ctx: at.ctx, ..span })
        }

        fn span_located_at(&mut self, a: u64, b: u64) -> u64 {
            let (Some(span), Some(other)) = (self.span(a), self.span(b)) else { return b };
            self.parts.pack(span::Span { ctx: span.ctx, ..other })
        }

        fn span_parent(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return NONE };
            self.ask.parent(span).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_source(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            self.ask.source(span).map_or(a, |it| self.parts.pack(it))
        }

        fn span_start(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            if span.anchor.ast_id == span::FIXUP_ERASED_FILE_AST_ID_MARKER {
                return a;
            }
            self.parts.pack(span::Span { range: TextRange::empty(span.range.start()), ..span })
        }

        fn span_end(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            if span.anchor.ast_id == span::FIXUP_ERASED_FILE_AST_ID_MARKER {
                return a;
            }
            self.parts.pack(span::Span { range: TextRange::empty(span.range.end()), ..span })
        }

        fn span_subspan(&mut self, a: u64, start: u64, end: u64) -> u64 {
            let Some(span) = self.span(a) else { return NONE };
            self.ask.subspan(span, start, end).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_pack(&mut self, start: u32, len: u32, part: u32) -> u64 {
            self.parts.pack_fields(start, len, part)
        }

        fn span_unpack(&mut self, a: u64) -> [u32; 3] {
            self.parts.fields(a)
        }

        fn span_byte_range(&mut self, a: u64) -> [u32; 2] {
            let Some(span) = self.span(a) else { return [0, 0] };
            self.ask.byte_range(span)
        }

        fn span_line(&mut self, a: u64) -> u32 {
            let span = self.span(a);
            span.and_then(|span| self.ask.line_column(span)).map_or(1, |(line, _)| line)
        }

        fn span_column(&mut self, a: u64) -> u32 {
            let span = self.span(a);
            span.and_then(|span| self.ask.line_column(span)).map_or(1, |(_, column)| column)
        }

        fn span_debug(&mut self, a: u64) -> Option<String> {
            // alloc: the text the macro asked for.
            self.span(a).map(|it| format!("{it:?}"))
        }

        fn span_file(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.file(span)
        }

        fn span_local_file(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.local_file(span)
        }

        fn span_source_text(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.source_text(span)
        }

        fn ts_from_str(&mut self, text: &str) -> Result<Vec<u8>, String> {
            let call_site = self.call_site_span();
            self.ask.ts_from_str(self.parts, text, call_site)
        }

        fn literal_from_str(&mut self, text: &str) -> Option<Vec<u8>> {
            let call_site = self.call_site_span();
            self.ask.literal_from_str(self.parts, text, call_site)
        }

        #[inline]
        fn pack(&mut self, span: span::Span) -> u64 {
            self.parts.pack(span)
        }

        #[inline]
        fn get(&self, word: u64) -> Option<span::Span> {
            self.parts.get(word)
        }
    }
```

(These are `Host`'s answers as `native/host.rs` gives them today, line for line, with the
database questions moved behind `ask`.)

> forwarded to usercode (requests, pass 22).
> applied (usercode-dedyn-native): `trait Ask` and the concrete `Host<'a>` are in
> `proc_macro/lib.rs` `__native` as written, the methods `pub` (the `sys` functions and
> `native/mod.rs` call them); the one `dyn` left is `ask`, with its `// dyn:` line. `sys` is
> unchanged, so the guest's text and `the_two_sys_modules_declare_the_same_functions` hold.

### 3. The thread-local and its two functions

- `static HOST: Cell<Option<*mut (dyn Host + 'static)>>` and its `// dyn:` line become
  `static HOST: Cell<Option<*mut Host<'static>>> = const { Cell::new(None) };`
- `with_host`: drop its three `// dyn:` lines; signature
  `pub fn with_host<R>(host: &mut Host<'_>, f: impl FnOnce() -> R) -> R`;
  `struct Restore(Option<*mut Host<'static>>);`; and in place of the `let host: *mut (dyn Host + '_)`
  and `transmute` lines:

```rust
        // SAFETY: only the lifetime changes. The pointer is read by `host` alone, on this
        // thread, while `f` runs: `Restore` puts the one before it back when `f` returns or
        // unwinds, before the borrow it came from ends.
        let host = std::ptr::from_mut(host).cast::<Host<'static>>();
```

  (the `SAFETY` text is today's; `cast` is not `unsafe`, so the comment sits on the deref in
  `host` below, where it already is; drop it here if the lint flags a `SAFETY` with no `unsafe`).
- `host`: drop its two `// dyn:` lines; signature
  `pub(crate) fn host<R>(f: impl FnOnce(&mut Host<'_>) -> R) -> R`;
  `struct Restore(*mut Host<'static>);`; the last line becomes
  `f(unsafe { &mut *host.cast::<Host<'_>>() })` with today's `SAFETY` comment, plus at its end:
  "`f` is given the host at a lifetime of its own, never `'static`."
- `unpacked(host: &mut dyn Host, ..)` → `unpacked(host: &mut Host<'_>, ..)`, and `push`'s
  `host: &mut dyn Host` → `host: &mut Host<'_>`; drop both `// dyn:` lines. `registered`,
  `word`, `built` and every `sys` function are unchanged (they call the same method names, now
  inherent).

> forwarded to usercode (requests, pass 22).
> applied (usercode-dedyn-native): `HOST` holds `*mut Host<'static>`, `with_host` and `host`
> take and give `&mut Host<'_>` with the `SAFETY` text on the deref in `host` only (the `cast`
> line keeps it as a plain comment); no `dyn` left on that path.

### 4. `crates/blaze-expand/src/native/host.rs`

- `pub(super) struct Host<'a> { db, parts, sites }` becomes

```rust
/// What only the database and the lexer answer of one expansion (`__native::Ask`).
pub(super) struct Ask<'a> {
    /// What is asked of a span, or `None` where nothing can be, as in a test.
    pub(super) db: Option<&'a Db>,
}
```

  with `fn ask(&mut self, req)` and `fn line_column(&mut self, span)` kept as inherent
  methods (`line_column` unchanged); drop `span`, `number`, `call_site_span`; `written`
  becomes a free function
  `fn written(parts: &mut Parts, trees: impl IntoIterator<Item = Tree>, call_site: Span) -> Vec<u8>`
  with the body `wire::relative(&wire::invisible(trees, call_site), parts, &mut out)`.
- `impl native_proc_macro::__native::Host for Host<'_>` becomes
  `impl native_proc_macro::__native::Ask for Ask<'_>` with exactly:

```rust
    fn join(&mut self, first: Span, second: Span) -> Option<Span> {
        spans::join(first, second, |req| self.ask(req))
    }

    fn parent(&mut self, span: Span) -> Option<Span> {
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanParent { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanParentResult { parent_span: Some(it) }) => {
                Some(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
            }
            _ => None,
        }
    }

    fn source(&mut self, span: Span) -> Option<Span> {
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanSource { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                Some(spanned(file_id, ast_id, start, end, ctx))
            }
            _ => None,
        }
    }

    fn subspan(&mut self, span: Span, start: u64, end: u64) -> Option<Span> {
        spans::subspan(span, start, end)
    }

    fn byte_range(&mut self, span: Span) -> [u32; 2] {
        let (file_id, ast_id, start, end) = located(span);
        let range = match self.ask(SubRequest::ByteRange { file_id, ast_id, start, end }) {
            Some(SubResponse::ByteRangeResult { range }) => range,
            _ => usize::from(span.range.start())..usize::from(span.range.end()),
        };
        [range.start as u32, range.end as u32]
    }

    fn line_column(&mut self, span: Span) -> Option<(u32, u32)> {
        Ask::line_column(self, span)
    }

    fn file(&mut self, span: Span) -> Option<String> {
        let req = SubRequest::FilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::FilePathResult { name }) => Some(name),
            // alloc: none, an empty name.
            _ => Some(String::new()),
        }
    }

    fn local_file(&mut self, span: Span) -> Option<String> {
        let req = SubRequest::LocalFilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        }
    }

    fn source_text(&mut self, span: Span) -> Option<String> {
        let (file_id, ast_id, start, end) = located(span);
        match self.ask(SubRequest::SourceText { file_id, ast_id, start, end }) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        }
    }

    fn ts_from_str(&mut self, parts: &mut Parts, text: &str, call_site: Span) -> Result<Vec<u8>, String> {
        match lex::stream(text, call_site) {
            Ok(trees) => Ok(written(parts, trees, call_site)),
            // alloc: the message of text refused.
            Err(e) => Err(format!("failed to parse str to token stream: {e}")),
        }
    }

    fn literal_from_str(&mut self, parts: &mut Parts, text: &str, call_site: Span) -> Option<Vec<u8>> {
        let literal = lex::literal_from_str(text, call_site)?;
        Some(written(parts, [Tree::Leaf(tt::Leaf::Literal(literal))], call_site))
    }
```

  (`span_parent` returned `NONE`, `span_source` `a`, on no answer: `Host` above keeps that by
  `map_or`.) Rename the inherent `line_column` to avoid the clash if the compiler asks
  (e.g. `asked_line_column`). Drop the now-unused `FIXUP_ERASED_FILE_AST_ID_MARKER`/`TextRange`
  imports and `NONE`.

> forwarded to usercode (requests, pass 22).
> applied (usercode-dedyn-native): `native/host.rs` is `Ask { db: Option<&Db> }` with the
> inherent `ask`, the free `written`, and `impl __native::Ask` as written; `line_column` lives
> in the trait impl alone (no inherent copy, so no clash to rename).

### 5. Callers

- `native/mod.rs` `expand`: `let mut host = Host { db, parts: &mut parts, sites };` becomes

```rust
    let mut ask = host::Ask { db };
    let mut host = native_proc_macro::__native::Host { parts: &mut parts, sites, ask: &mut ask };
```

  and `use self::host::Host;` goes.
- `native/tests.rs:51`: `let mut host = host::Host { db: None, parts: &mut parts, sites: words };`
  becomes the same two lines with `db: None` and `sites: words`.

Tests: the existing `native/tests.rs` differential tests (bridge output equal to the wasm
module's, span for span) cover it unchanged; nothing new to pin.

> forwarded to usercode (requests, pass 22).
> applied (usercode-dedyn-native): `native/mod.rs:62-63` and `native/tests.rs:51-52` build
> `host::Ask` then `__native::Host`; `use self::host::Host;` is gone. No edit to `lib.rs` was
> needed.

## usercode-hot-macros, 2026-10-29: a kept output read off a 4-byte boundary copies into new memory

Where: `crates/blaze-expand/src/bridge/wire.rs`, `read` (its `let mut room = Vec::new();` and
`aligned`). Owner: usercode (`crates/blaze-expand/`, outside `native/`).

What: a `read_in(bytes: &[u8], parts: &Parts, room: &mut Vec<u8>) -> Result<TopSubtree, String>`
that is today's `read` with the caller's `room` (`read` stays, calling it with a new one).
`crates/blaze-macro-wasm/src/lib.rs` `Shapes::read` then passes a room kept in its `Scratch`
(next to `kept`), so a store hit copies into reused memory.

Why: a hit reads the kept tree in place in the store, under the guard. The tree starts
`4 + 8 × parts` bytes into the value, so it is aligned exactly when the store's value is. When it
isn't, `aligned` allocates a copy on every hit: one allocation beyond what the output owns on the
hit path the brief says allocates nothing else. The budget test
`a_kept_expansion_is_found_with_nothing_allocated_but_its_output` allows that one copy, counted
from the value's address, and goes to the output's count alone once `read_in` is used.

> forwarded to usercode (told by tmux; requests, pass 23).

> applied (usercode, 2026-10-29):
> - `bridge::wire::read_in(bytes, parts, room)` is today's `read` with the caller's room, and
>   `read` calls it with a new one.
> - `Shapes::read` takes the room, and `Scratch` keeps it as `aligned: Vec<u8>`. `expand` lends
>   it through a `Cell`, as it does `kept`.
> - The budget test drops the copy: a hit is held to the output's count alone. The address
>   probe it used for the copy (`find_answered` with a fresh `Room`) is gone.

## usercode-hot-macros, 2026-10-29: `Inputs::env_all` lends owned strings

Where: `crates/blaze-usercode/src/trace.rs`, `trait Inputs`, `fn env_all(&mut self) ->
&[(OsString, OsString)]`. Owner: usercode (`crates/blaze-usercode/`).

What: lend borrowed pairs, `&[(&OsStr, &OsStr)]`, as
`env` already lends `Option<&OsStr>`.

Why: the wasm host's environment is `&[(&str, &str)]` made from the session's `Env` with no
copy. To lend it as `env_all` asks, `Now::env_all` (`crates/blaze-macro-wasm/src/lib.rs`) owns a
copy in thread-local room, rebuilt only when the environment differs from the thread's last
lookup: a run that read its whole environment (rare: a module calling `env::vars`) allocates
the copy on the first lookup of each other environment. With borrowed pairs it lends the slice
as it is.

> forwarded to usercode (told by tmux; requests, pass 23).

> declined (usercode, 2026-10-29): it moves the copy rather than removing it.
> - `env_all` has 20 implementations, in blaze-usercode, blaze-expand, proc-macro-api, the
>   tests and two other lanes' tests.
> - All but the wasm host own `(OsString, OsString)` pairs: `inputs::Env`, blaze-expand's
>   `Seen`, the traces. None of them can lend `&[(&OsStr, &OsStr)]` without building a second
>   vector of borrows into itself, an allocation on every call where today there is none.
> - The one case it helps is rare and already bounded: a wasm module that read its whole
>   environment copies it once per thread per distinct environment, and never on a repeat
>   lookup.
> - The shape that would fit every implementor is an iterator over sorted borrowed pairs:
>   `fn env_all(&mut self) -> impl Iterator<Item = (&OsStr, &OsStr)> + Clone`. `keyed` reads it
>   twice (`names_hex`, then the merge), and `Clone` allows that. If that copy ever shows up in
>   a profile, ask for that, and usercode makes the change across the trait.

## usercode-hot-macros, 2026-10-29: the answer to a span question owns its text

Where: `crates/load-cargo/src/lib.rs`, `SubResponse` (`FilePathResult { name: String }`,
`SourceTextResult { text: Option<String> }`, `LocalFilePathResult { name: Option<String> }`),
which `blaze-macro-wasm`'s `answer` gets from the database. Owner: not this lane's.

What, and why: nothing to do now. A hit whose key asked a span's file or source text allocates
that answer's `String` once per question in `answer`, before it is hashed (`keyed::hex` into
reused room). A `SubResponse` lending `&str` from the database would make those hits allocate
nothing; noted so the owner can weigh it, not asked for.

> forwarded to cargo (`crates/load-cargo/`; it says nothing to do now: noted) (requests, pass 23).
