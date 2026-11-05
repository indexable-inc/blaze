---
status: record
summary: blaze doc as blaze v1 did it (the engine's doc.rs, over a workspace loaded into rust-analyzer): HTML documentation written from the front end's HIR, with no rustdoc. v2 has no `blaze doc` yet.
date: 2026-10-30
---
# `blaze doc`

`blaze doc` (and `blaze doc`) writes the HTML documentation of the packages asked for and
of what they depend on, as `cargo doc` does, with no rustdoc: blaze writes every page from
rust-analyzer's HIR (`crates/blaze-doc`). The pages are not rustdoc's byte for byte; they hold
what rustdoc's hold, look much like them, and work the same way (search, sidebar, themes,
source pages).

```sh
blaze doc --open            # the package and its dependencies, then open its page
blaze doc --no-deps -p foo  # one package, nothing it depends on
```

## What it writes

`target/doc/<crate>/index.html` for each crate, or `target/<triple>/doc/...` with `--target`,
and beside them:

- a page for every item rustdoc gives one (modules, structs, enums, unions, traits, trait
  aliases, functions, type aliases, constants, statics, macros, derive and attribute macros,
  primitives a crate documents), at rustdoc's file names, so a link into rustdoc's docs works
  into blaze's;
- on each: the declaration with every name linked to its page, fields and variants, inherent
  impls, trait impls (written, blanket and auto ones), "Methods from `Deref<Target = ..>`",
  implementors of a trait (from every crate documented, as rustdoc's script adds them), the
  members an impl leaves as the trait provides them, and notes for `#[deprecated]`,
  `#[must_use]`, `unsafe` and `#[doc(cfg)]`; a type alias shows the type it stands for with
  its fields or variants ("Aliased Type");
- re-exports inlined or listed as rustdoc decides (`#[doc(inline)]`, `#[doc(no_inline)]`,
  `#[doc(hidden)]`, globs, private modules, a chain through another public `pub use`);
- docs as rustdoc renders Markdown: pulldown-cmark with rustdoc's options, headings with
  anchors, the first paragraph as the summary in lists and search, Rust examples highlighted
  with their `# ` lines hidden, and intra-doc links resolved by rust-analyzer; a link that
  resolves to nothing prints `warning: unresolved link to `...`` as rustdoc does, for the
  packages asked for and those from a path (cargo caps the lints of registry and git ones),
  unless an `allow` or `expect` of `rustdoc::broken_intra_doc_links` (or `rustdoc::all`,
  `warnings`) is on the item or around it, `cfg_attr` read with the crate's cfg, or a macro of
  another crate (or an attribute or derive macro) wrote the doc attribute, where rustc
  reports no lint;
- links to an item of a crate not documented here go where its `#![doc(html_root_url)]` says,
  at the path the item is declared at, as rustdoc's do; with no `html_root_url` the name is
  not a link;
- a source page for every file, linked from each item's `Source` link;
- an "All items" page per crate, the module list in the sidebar, a search box (`s` or `/`)
  over every crate documented, light and dark themes following the system's.

It prints `Documenting`, `Finished` and `Generated` as cargo does; `-v` says how long the load,
the model and the writing took.

The flags are cargo's: `-p`, `--workspace`, `--exclude`, `--no-deps`,
`--document-private-items`, `--open`, `--features`, `--all-features`, `--no-default-features`,
`--target`, `--target-dir`, `--manifest-path`, `--lib`, `--bins`, `--bin`, `--release`,
`--profile`, `-q`, `-v`. A binary is documented with its private items, as cargo does. A
dev-dependency is not documented, nor linked into the crate it is a dev-dependency of (its
library's docs have no tests in them), though rust-analyzer's crate graph gives the library
it. Two dependencies of one name share a directory, so one is documented, as cargo's
`remove_duplicate_doc` picks it: none that shares a name with a package asked for; of several,
none built only for the host (a proc macro and what it depends on), so two versions of `syn`
leave `syn` undocumented; then the newest version. A crate only a left-out one depends on is
left out with it (helix's `ahash`, which only the older `hashbrown` uses), as cargo drops the
units nothing reaches any more.

## How

Three passes over one loaded database, each parallel over the crates:

1. `places` walks every module tree as rustdoc does and gives each item its page: where it is
   declared, or where a public re-export inlines it, under the crate that places it.
2. `collect` builds the model of each crate (`model.rs`): declarations as code with links,
   docs with links resolved, members, impls. Nothing in the model is HTML.
3. `html` writes the pages, source pages, sidebars and search index from the model alone.

Which provided methods an impl shows follows rustc: a method whose where-clauses on the
trait's parameters cannot hold for the impl (`rev` of an iterator that is not a
`DoubleEndedIterator`) is left out, by a port of rustc's `is_impossible_associated_item` into
rust-analyzer's solver (`hir_ty::traits::is_impossible_associated_item`). Blanket impls come
from every crate linked into the documented one, documented or not (not a proc macro's
dependencies, which rustc loads as the compiler's own), and apply as rustdoc's
`synthesize_blanket_impls` asks (`hir_ty::traits::impl_may_apply`), solved in the documented
crate: a type it re-exports gets the impls of every crate it links, not only those its own
crate sees (`hashbrown`'s `Equivalent` on `gix_diff::blob::Algorithm`, from `imara_diff`).

### The load is one-shot, in the command's process

`blaze doc` loads the workspace in its own process, as the daemon's checker loads it
(`crate::load` in `crates/blaze-engine`, from `crates/blaze-engine/src/doc.rs`), writes the pages
and exits. It does not ask the daemon: a database for documentation needs every dependency's
items and bodies' signatures, with `cfg(doc)` set, and a daemon would hold that only for `doc`
to read once; the one-shot load needs no daemon running and leaves nothing behind.

### What still runs cargo

The pages are blaze's, and no rustdoc or rustc runs to write them. The load the command shares
with the checker still runs cargo in two places, and this command adds no new one:

- build scripts and proc macros: the load builds and runs them through cargo
  (`ProjectWorkspace::run_build_scripts`) so a crate that `include!`s its `OUT_DIR` or derives
  through a proc macro reads as it compiles. That is most of a first run's wall time.
- the sysroot: unless the toolchain is known, rust-analyzer asks `rustc --print sysroot` and
  cargo where the standard library's source is (`RustLibSource::Discover`).

Both go when the load stops needing them ([no-rustc.md](no-rustc.md)).

## Differences from rustdoc

- Smart punctuation is on, as in rustdoc's `main_body_opts` (`--` becomes an en dash, quotes
  curl); the brief asked for it off.
- A link to an item another documented crate places points straight at that crate's page.
  rustdoc points at the path the item is defined at and writes a redirect page there; blaze
  writes no redirect pages, and names the defined path in the link's title as rustdoc does.
  (Links to a crate not documented here, which blaze writes no page for, go to the defined
  path under its `html_root_url`, as rustdoc's do, since that crate's own docs redirect.)
- With `--target`, cargo writes the crates built only for the host (proc macros and what they
  depend on) to `target/doc`, apart from the rest in `target/<triple>/doc`; blaze writes one
  directory and drops a host crate whose name another crate has, as cargo does with no
  `--target`.
- Lint levels given on the command line (`RUSTDOCFLAGS="-A rustdoc::..."`) are not read.
- Implementors from crates downstream of a trait's crate are written into the trait's page,
  where rustdoc adds them with a script (`implementors/*.js`); a reader sees the same list.
- The stylesheet and script are blaze's own (`crates/blaze-doc/static`), with rustdoc's class
  names and colors and system fonts; the theme follows the system, with no settings page.

## How complete it is

Until 2026-10-30 a harness (deleted with the other comparisons with rustc and cargo,
[harness.md](harness.md)) ran the pinned nightly's `cargo doc` and `blaze doc` with the same
flags and compared, for each crate both documented: the item pages, the members and
impls anchored on pages both have (`method.new`, `impl-Debug-for-Foo`), the intra-doc links
rustdoc resolved (by their titles), and the `unresolved link` warnings. On the bench apps (each
with its dependencies; 2026-10-26, dev-aws-8, blaze at 69cbce21f5):

| App | Pages | Members and impls | Resolved links | Warnings rustdoc gives, blaze gives too | `cargo doc` | `blaze doc` |
|---|---|---|---|---|---|---|
| fd | 12785 of 12787 (100.0%) | 91972 of 92078 (99.9%) | 13340 of 13420 (99.4%) | 1 of 1 | 27.4 s | 7.7 s |
| ripgrep | 8037 of 8042 (99.9%) | 58477 of 58705 (99.6%) | 6527 of 6588 (99.1%) | 0 of 0 | 5.6 s | 5.9 s |
| nushell | 60756 of 61083 (99.5%) | 589898 of 594585 (99.2%) | 77494 of 78793 (98.4%) | 2 of 3 | 497.1 s | 28.4 s |
| helix | 28164 of 28476 (98.9%) | 376345 of 380019 (99.0%) | 54300 of 55013 (98.7%) | 0 of 0 | 149.2 s | 19.0 s |
| biome | 41563 of 42003 (99.0%) | 1770723 of 1784684 (99.2%) | 218253 of 222469 (98.1%) | 22 of 22, and 3 more | 1028.0 s | 92.5 s |

`cargo doc` starts from an empty target directory; `blaze doc` with its build scripts
already run (the first load also runs them through cargo: fd's first run took 56.6 s, nearly
all of it that). fd's 7.6 s, at an earlier commit, was 0.75 s of load, 6.3 s of model and
0.06 s of writing. helix ran with `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1` on both sides (its
build script otherwise fetches tree-sitter grammars), and biome with `BLAZE_SANDBOX=off`
(its build scripts write outside the sandbox blaze runs them in).

What blaze still misses or adds, and why:

- Impls a `macro_rules!` writes for a type alias (`impl Deserialize for AtomicBool` in serde,
  `impl AsRef<AbsolutePath> for ...` in nu-path) are named by the alias in blaze and by the
  type it stands for in rustdoc (`Atomic<bool>`), so their anchors differ.
- Implementors from crates downstream of the trait's are on the trait's page in blaze and
  added by rustdoc's script, which the comparison does not run: most of the "extra" anchors.
- A crate that re-exports whole modules of the standard library (`is_debug`, `shadow_rs`
  re-export `core::fmt` and the preludes) gets pages and auto-trait impls rustdoc leaves to
  redirects.
- Macros a `macro_rules!` defines inside another macro's expansion (icu's baked data
  `impl_*_v1!`) have no page.
- rustdoc writes pages under a directory named `self` (`futures/prelude/self/enum.Either.html`,
  `notify/self/...`, `crossbeam/self/fn.scope.html`), and blaze writes none there; why rustdoc
  does is not looked into yet.
- nushell's `tz`, `tzdb` and `tzdb_data` are documented by blaze and not by cargo: a feature
  enabled only through a build-dependency (resolver 2 keeps those apart) pulls them in, and
  `cargo metadata`, which the load reads, merges the two.
- rustdoc warns of `std::io::Error::other` in nushell and blaze does not; biome gets two
  warnings of `[0]` blaze cannot place in the source and one more of `serde::Deserialize`
  than rustdoc gives. Not looked into yet.
- blaze reads Markdown with pulldown-cmark 0.9.6, the workspace's (rust-analyzer's hover uses
  it too); rustdoc's is newer, and reads some nested brackets (`[[Call]]`) differently.

## Not done

- Doctests are `blaze test --doc`'s, not this command's.
- `--message-format` is refused; `--examples`, `--tests` and `--benches` are not read.
- No settings page and no `help.html`; the theme follows the system's. Links into the
  standard library point at doc.rust-lang.org's nightly docs, as rustdoc's do with no local
  copy; a crate's `html_root_url` is read, but there is no `--extern-html-root-url`.
