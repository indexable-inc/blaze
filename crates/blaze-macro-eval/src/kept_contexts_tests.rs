//! A kept proc-macro expansion holds no raw syntax-context word (port decision 3: a context
//! word is crate-local and follows interning order). Its spans name the contexts of its call by
//! ordinal (`proc_macro_api::kept::written`), which a reader resolves against its own call,
//! whose contexts it has interned in its own arena. A proc macro makes no context: every
//! context of its output is one of its input's, so the call's own contexts are the table.

use std::{alloc::Global, sync::Arc};

use base_db::Env;
use blaze_ids::ContentId;
use blaze_mem::Tag;
use blaze_memo::{Alone, ArenaId, Config, Gid, Memo, Plant, Reads};
use hir_expand::proc_macro::ProcMacroKind;
use proc_macro_api::kept::{Call, Spans, given, read, written};
use span::{
    Edition, EditionedFileId, ErasedFileAstId, MacroCallId, Span, SpanAnchor, SyntaxContext,
    TextRange, Transparency,
};

struct TestDb(Memo);

impl blaze_memo::Database for TestDb {
    fn memo(&self) -> &Memo {
        &self.0
    }
}

/// A fresh session (a process of its own) with one crate arena: the database and the arena.
fn session() -> (TestDb, ArenaId) {
    let mut builder = Memo::builder(Config {
        arenas: 2,
        reads: Reads::Off,
        flight: Arc::new_in(Alone::new(), Global),
        cap_bytes: u64::MAX,
        plant: Plant::None,
    });
    builder.arena(0, Tag::Query);
    let krate = builder.arena(1, Tag::Query).id();
    (TestDb(builder.build()), krate)
}

/// A call of `arena`'s, stored as a context's expansion and never looked up.
fn call(arena: ArenaId, n: u32) -> MacroCallId {
    MacroCallId(Gid::in_arena(arena, n))
}

fn span(start: u32, end: u32, ctx: SyntaxContext) -> Span {
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor {
            file_id: EditionedFileId::from_raw(1),
            ast_id: ErasedFileAstId::from_raw(0),
        },
        ctx,
    }
}

/// `struct <name>;` with the name spanned in `ctx` and the rest at the root.
fn tree(name: &str, ctx: SyntaxContext) -> tt::TopSubtree {
    let at = span(0, 9, SyntaxContext::root(Edition::CURRENT));
    tt::TopSubtree::invisible_from_leaves(
        at,
        [
            tt::Leaf::Ident(tt::Ident::new("struct", at)),
            tt::Leaf::Ident(tt::Ident::new(name, span(7, 8, ctx))),
            tt::Leaf::Punct(tt::Punct { char: ';', spacing: tt::Spacing::Alone, span: at }),
        ],
    )
}

fn spans_of(input: &tt::TopSubtree, env: &Env) -> Spans {
    let site = span(0, 9, SyntaxContext::root(Edition::CURRENT));
    let call = Call {
        name: "Serialize",
        kind: ProcMacroKind::CustomDerive,
        subtree: input,
        attrs: None,
        env,
        def_site: site,
        call_site: site,
        mixed_site: site,
        current_dir: "/",
    };
    given(&ContentId::of("test toolchain", b"nightly"), &call).1
}

/// The invariant: an expansion kept with a non-root context of one session, read back for the
/// same call in a fresh session whose arena numbers its contexts otherwise, gives the same tree
/// with that session's context of equal structure (expansion, transparency, edition, parent).
/// The control: the first session's raw word read in the fresh arena names another context.
#[test]
fn a_kept_expansion_reads_back_its_contexts_by_structure_in_a_fresh_arena() {
    let env: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
    let root = SyntaxContext::root(Edition::Edition2021);

    let (one, at_one) = session();
    let ctx_one = SyntaxContext::new(
        &one,
        at_one,
        call(at_one, 2),
        Transparency::SemiOpaque,
        Edition::Edition2021,
        root,
        None,
        None,
    );
    let spans_one = spans_of(&tree("A", ctx_one), &env);
    let bytes = written(&tree("Impl", ctx_one), &spans_one).expect("its contexts are the call's");

    // The fresh session interns a context of its own first, so the call's context takes
    // another index (and another word) there.
    let (two, at_two) = session();
    let own = SyntaxContext::new(
        &two,
        at_two,
        call(at_two, 7),
        Transparency::Transparent,
        Edition::Edition2021,
        root,
        None,
        None,
    );
    let ctx_two = SyntaxContext::new(
        &two,
        at_two,
        call(at_two, 2),
        Transparency::SemiOpaque,
        Edition::Edition2021,
        root,
        None,
        None,
    );
    assert_ne!(ctx_two.into_u32(), ctx_one.into_u32(), "the fresh arena numbers it otherwise");
    let spans_two = spans_of(&tree("A", ctx_two), &env);
    let back = read(&bytes, &spans_two).expect("kept against the same call");

    let same = |a: &tt::TopSubtree, b: &tt::TopSubtree| format!("{a:?}") == format!("{b:?}");
    assert!(same(&back, &tree("Impl", ctx_two)), "the reading session's context: {back:?}");
    assert_eq!(ctx_two.outer_mark(&two, at_two), (Some(call(at_two, 2)), Transparency::SemiOpaque));
    assert_eq!(ctx_two.parent(&two, at_two), root);
    assert_eq!(ctx_two.edition(), Edition::Edition2021);

    // The control: had the record held the raw word, the fresh arena would read `own`.
    let raw = SyntaxContext::from_u32(ctx_one.into_u32());
    assert_eq!(raw.outer_mark(&two, at_two), own.outer_mark(&two, at_two));
    assert_ne!(raw.outer_mark(&two, at_two), ctx_two.outer_mark(&two, at_two));
    assert!(!same(&back, &tree("Impl", raw)));
}
