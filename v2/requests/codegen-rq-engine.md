# Requests of codegen-rq-engine (lane codegen)

## 1. `crates/blaze-cargo/src/errors.rs` (cargo): a diagnostic carries rustc's whole text

What: `errors::Diagnostic` gains the text blaze-diagnostics wrote of it, and the three renderers
give that text when it is there.

`errors.rs:12-19`, before:

```rust
/// An error, and where it is when it is of one place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// rustc's, as `E0308`, for the ones that have one.
    pub code: Option<String>,
    pub message: String,
    pub span: Option<Span>,
}
```

after:

```rust
/// An error, and where it is when it is of one place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// rustc's, as `E0308`, for the ones that have one.
    pub code: Option<String>,
    pub message: String,
    pub span: Option<Span>,
    /// rustc's whole text of it, with every span, label and note, where the one who found it
    /// wrote it (blaze-diagnostics): what [`Diagnostic::rendered`], [`Diagnostic::rendered_ansi`]
    /// and [`Diagnostic::short`] give then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered: Option<Box<Rendered>>,
}

/// A diagnostic as rustc writes it, in each form cargo asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rendered {
    pub plain: String,
    /// `--color=always`, `json-diagnostic-rendered-ansi`.
    pub ansi: String,
    /// `--message-format=short`, `json-diagnostic-short`.
    pub short: String,
}
```

`errors.rs:80` and `85` (the literals in `at` and `of_none`), and the test literal at `414`:
add `rendered: None`.

At the top of `rendered` (`errors.rs:95`), `rendered_ansi` (`120`) and `short` (`152`), in that
order:

```rust
        if let Some(rendered) = &self.rendered {
            return rendered.plain.clone();
        }
```

```rust
        if let Some(rendered) = &self.rendered {
            return rendered.ansi.clone();
        }
```

```rust
        if let Some(rendered) = &self.rendered {
            return rendered.short.clone();
        }
```

Why: diagnostics item 3 (`v2/requests/diagnostics.md`, #1522, #1440, #780). blaze-engine refuses
a build for every error blaze-diagnostics gives in its crates, and must say each as rustc does.
`errors.rs`'s renderer writes one span and no children, so `blaze-engine/src/lib.rs` `said_of`
hands it `blaze_diagnostics::emit::rendered`'s text in the three styles as `Rendered`. It already
does: blaze-engine does not build until this is in. `serde(default)` keeps older replies
(`Said::of_reply`) reading. blaze-engine's own literals (`check.rs`, `lib.rs`) are written as
`Diagnostic { code, ..of_none(..) }`, so they build with the field and without it.

Test (blaze-engine, `lib.rs`): `an_edit_is_refused_for_every_error_of_its_crate_as_rustc_writes_it`
reads `rendered.{plain, short, ansi}`.

Status: open

> forwarded to cargo-next2 (requests, pass 38: `blaze-cargo` is cargo's and lane cargo runs; told by tmux)
> applied by lane cargo (cargo-next2, 2026-10-29, not built): as written. `Diagnostic::rendered: Option<Box<Rendered>>` (`serde(default, skip_serializing_if)`) and `Rendered { plain, ansi, short }` are in `errors.rs`. `rendered: None` is in `at`, `of_none` and the test literal, and `rendered`, `rendered_ansi` and `short` each return their form first when it is there. No other crate builds an `errors::Diagnostic` literal without `..of_none(..)`.

## 2. Root `Cargo.toml` (coordinator) and `crates/ide-diagnostics` (diagnostics): nothing uses it

What: `Cargo.toml:156` `ide-diagnostics = { path = "crates/ide-diagnostics", version = "0.0.0" }`
goes, and lane diagnostics deletes `crates/ide-diagnostics`, as diagnostics item 3 says.

Why: blaze-engine was its last user; its manifest now names `blaze-diagnostics` in its place.
`rg ide_diagnostics crates` finds only the module doc of `crates/ide-db/src/assists.rs:3,5`
outside the crate itself: words, no use.

Status: open

> applied (requests, pass 38): the root `ide-diagnostics` line is gone (no manifest names it; `crates/ide-diagnostics` stays a `crates/*` member until deleted, needing no root line). The crate's deletion forwarded to diagnostics (diagnostics-levels told). Manifest lint clean.
