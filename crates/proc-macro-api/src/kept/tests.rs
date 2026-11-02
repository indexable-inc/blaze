use std::path::PathBuf;

use base_db::Env;
use blaze_core::{Traced, Untraceable};
use blaze_ids::ContentId;
use blaze_usercode::{RunKey, index};
use hir_expand::proc_macro::ProcMacroKind;
use span::{EditionedFileId, Edition, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange};

use super::{CACHE, Call, LIBRARY, Mode, given, read, written};

fn span(file: u32, ast: u32, start: u32, end: u32) -> Span {
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor {
            file_id: EditionedFileId::from_raw(file),
            ast_id: ErasedFileAstId::from_raw(ast),
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn tree(name: &str, at: Span) -> tt::TopSubtree {
    tt::TopSubtree::invisible_from_leaves(
        at,
        [
            tt::Leaf::Ident(tt::Ident::new("struct", at)),
            tt::Leaf::Ident(tt::Ident::new(name, Span { range: TextRange::new(7.into(), 8.into()), ..at })),
            tt::Leaf::Punct(tt::Punct { char: ';', spacing: tt::Spacing::Alone, span: at }),
        ],
    )
}

struct Inputs {
    subtree: tt::TopSubtree,
    env: Env,
    site: Span,
    dir: &'static str,
    name: &'static str,
}

impl Inputs {
    fn new() -> Inputs {
        Inputs {
            subtree: tree("A", span(1, 0, 0, 9)),
            env: [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect(),
            site: span(1, 0, 0, 9),
            dir: "/work/a",
            name: "Serialize",
        }
    }

    fn key(&self) -> ContentId {
        let call = Call {
            name: self.name,
            kind: ProcMacroKind::CustomDerive,
            subtree: &self.subtree,
            attrs: None,
            env: &self.env,
            def_site: self.site,
            call_site: self.site,
            mixed_site: self.site,
            current_dir: self.dir,
        };
        given(&ContentId::of("test toolchain", b"nightly"), &call).0.finish()
    }
}

#[test]
fn equal_calls_give_equal_keys() {
    assert_eq!(Inputs::new().key(), Inputs::new().key());
}

#[test]
fn the_same_call_in_another_file_or_item_gives_the_same_key() {
    let mut moved = Inputs::new();
    moved.site = span(7, 3, 0, 9);
    moved.subtree = tree("A", span(7, 3, 0, 9));
    assert_eq!(moved.key(), Inputs::new().key());
}

#[test]
fn a_changed_token_gives_another_key() {
    let mut changed = Inputs::new();
    changed.subtree = tree("B", changed.site);
    assert_ne!(changed.key(), Inputs::new().key());
}

#[test]
fn a_changed_span_gives_another_key() {
    let mut changed = Inputs::new();
    changed.subtree = tree("A", span(1, 0, 1, 9));
    assert_ne!(changed.key(), Inputs::new().key());
    // Two anchors where there was one is another call too.
    let mut split = Inputs::new();
    split.site = span(1, 0, 0, 9);
    split.subtree = tree("A", span(1, 5, 0, 9));
    assert_ne!(split.key(), Inputs::new().key());
}

#[test]
fn a_changed_variable_name_or_directory_gives_another_key() {
    let mut env = Inputs::new();
    env.env = [("CARGO_PKG_NAME".to_owned(), "b".to_owned())].into_iter().collect();
    let mut name = Inputs::new();
    name.name = "Deserialize";
    let mut dir = Inputs::new();
    dir.dir = "/work/b";
    let key = Inputs::new().key();
    for other in [env, name, dir] {
        assert_ne!(other.key(), key);
    }
}

#[test]
fn a_changed_traced_file_or_variable_gives_another_result_key() {
    let run = RunKey { code: ContentId::of(LIBRARY, b"lib"), given: Inputs::new().key() };
    let read = |hash: u8, value: &str| Traced {
        files: vec![(PathBuf::from("/work/a/schema.json"), Some([hash; 32]))],
        env: vec![("PATH".into(), Some(value.into()))],
        ..Traced::default()
    };
    let key = index::result_key(&run, &read(1, "/bin"));
    assert_eq!(index::result_key(&run, &read(1, "/bin")), key);
    assert_ne!(index::result_key(&run, &read(2, "/bin")), key);
    assert_ne!(index::result_key(&run, &read(1, "/usr/bin")), key);
    let other = RunKey { code: ContentId::of(LIBRARY, b"lib 2"), ..run };
    assert_ne!(index::result_key(&other, &read(1, "/bin")), key);
}

#[test]
fn an_expansion_that_read_the_clock_is_not_keepable() {
    let traced = Traced { untraceable: vec![Untraceable::Clock], ..Traced::default() };
    assert!(index::untraceable(&traced));
}

#[test]
fn an_output_is_written_against_its_calls_spans_and_read_back_in_another() {
    let inputs = Inputs::new();
    let call = |subtree: &tt::TopSubtree, site: Span, env: &Env| {
        let call = Call {
            name: "Serialize",
            kind: ProcMacroKind::CustomDerive,
            subtree,
            attrs: None,
            env,
            def_site: site,
            call_site: site,
            mixed_site: site,
            current_dir: "/",
        };
        given(&ContentId::of("test toolchain", b"nightly"), &call).1
    };
    let spans = call(&inputs.subtree, inputs.site, &inputs.env);
    let output = tree("Impl", inputs.site);
    let bytes = written(&output, &spans).expect("its spans are the call's");
    let same = |a: &tt::TopSubtree, b: &tt::TopSubtree| format!("{a:?}") == format!("{b:?}");
    assert!(same(&read(&bytes, &spans).unwrap(), &output));
    let elsewhere = span(9, 2, 0, 9);
    let there = call(&tree("A", elsewhere), elsewhere, &inputs.env);
    assert!(same(&read(&bytes, &there).unwrap(), &tree("Impl", elsewhere)));
    assert!(!same(&read(&bytes, &there).unwrap(), &output));
    // An output with an anchor its call has not can't be given back to another call.
    assert!(written(&tree("Impl", span(1, 4, 0, 9)), &spans).is_none());
    assert!(read(b"not an expansion", &spans).is_err());
}

fn derive<'a>(subtree: &'a tt::TopSubtree, attrs: Option<&'a tt::TopSubtree>, env: &'a Env, site: Span) -> Call<'a> {
    Call {
        name: "Serialize",
        kind: ProcMacroKind::CustomDerive,
        subtree,
        attrs,
        env,
        def_site: site,
        call_site: site,
        mixed_site: site,
        current_dir: "/",
    }
}

/// A hit keys its call and reads the kept output back with no allocation but the output's
/// tree: the columns, shapes and parts are the thread's, kept from the call before.
#[test]
fn a_hit_is_keyed_and_read_with_only_its_tree_allocated() {
    let inputs = Inputs::new();
    let toolchain = ContentId::of("test toolchain", b"nightly");
    let call = derive(&inputs.subtree, None, &inputs.env, inputs.site);
    let bytes = written(&tree("Impl", inputs.site), &given(&toolchain, &call).1).unwrap();
    let mut hit = || {
        let (given, spans) = given(&toolchain, &call);
        (given.finish(), read(&bytes, &spans).unwrap())
    };
    let first = hit();
    let again = blaze_alloc_count::assert_at_most(1, "a hit's key and read", &mut hit);
    assert_eq!(first.0, again.0);
    assert_eq!(format!("{:?}", first.1), format!("{:?}", again.1));
}

/// A site too wide to pack in a span word is still keyed by its range: two calls whose call
/// sites differ only there are two keys.
#[test]
fn a_wide_site_is_keyed_by_its_range() {
    let inputs = Inputs::new();
    let toolchain = ContentId::of("test toolchain", b"nightly");
    let key = |start: u32| {
        let site = span(1, 0, start, start + (1 << 17));
        given(&toolchain, &derive(&inputs.subtree, None, &inputs.env, site)).0.finish()
    };
    assert_eq!(key(0), key(0));
    assert_ne!(key(0), key(3));
}

#[test]
fn an_attribute_none_and_an_empty_one_are_other_calls() {
    let inputs = Inputs::new();
    let toolchain = ContentId::of("test toolchain", b"nightly");
    let empty = tt::TopSubtree::empty(tt::DelimSpan::from_single(inputs.site));
    let key = |attrs| given(&toolchain, &derive(&inputs.subtree, attrs, &inputs.env, inputs.site)).0.finish();
    assert_ne!(key(None), key(Some(&empty)));
}

/// An output with a span part the call has not in another place, or a part twice, or bytes cut
/// short, is refused by name rather than read as another tree.
#[test]
fn a_kept_value_that_is_not_one_is_refused() {
    let inputs = Inputs::new();
    let toolchain = ContentId::of("test toolchain", b"nightly");
    let spans = given(&toolchain, &derive(&inputs.subtree, None, &inputs.env, inputs.site)).1;
    let bytes = written(&tree("Impl", inputs.site), &spans).unwrap();
    for cut in [0, 3, 4, bytes.len() - 1] {
        assert!(read(&bytes[..cut], &spans).is_err(), "{cut}");
    }
    let mut twice = bytes[..bytes.len() - 4].to_vec();
    twice.extend_from_slice(&bytes[bytes.len() - 12..bytes.len() - 4]);
    twice.extend_from_slice(&2u32.to_le_bytes());
    assert!(read(&twice, &spans).unwrap_err().contains("earlier one again"));
    let mut elsewhere = bytes.clone();
    let at = elsewhere.len() - 12;
    elsewhere[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
    assert!(read(&elsewhere, &spans).unwrap_err().contains("anchor or context"));
}

#[test]
fn the_cache_has_one_mode_besides_keeping() {
    assert_eq!(Mode::of(None), Ok(Mode::Keep));
    assert_eq!(Mode::of(Some("check")), Ok(Mode::Check));
    for old in ["off", "0", "all", "on"] {
        assert!(Mode::of(Some(old)).unwrap_err().contains(CACHE));
    }
}
