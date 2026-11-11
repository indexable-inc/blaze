use std::path::PathBuf;

use base_db::Env;
use blaze_core::{Traced, Untraceable};
use blaze_ids::ContentId;
use blaze_usercode::{RunKey, index};
use hir_expand::proc_macro::ProcMacroKind;
use span::{Edition, EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange};

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
            tt::Leaf::Ident(tt::Ident::new(
                name,
                Span { range: TextRange::new(7.into(), 8.into()), ..at },
            )),
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

/// The calling crate's variables and directory are written against its package and
/// `OUT_DIR`: the same call on the same crate in another checkout has the same key.
#[cfg(not(windows))] // Windows has no roots (`blaze_usercode::roots`).
#[test]
fn the_same_call_in_another_checkout_gives_the_same_key() {
    let at = |home: &str| {
        let mut call = Inputs::new();
        call.env = [
            ("CARGO_PKG_NAME".to_owned(), "a".to_owned()),
            ("CARGO_MANIFEST_DIR".to_owned(), format!("{home}/p")),
            ("OUT_DIR".to_owned(), format!("{home}/target/debug/build/p-1/out")),
            ("INCLUDE".to_owned(), format!("-I{home}/p/include")),
        ]
        .into_iter()
        .collect();
        call.dir = Box::leak(format!("{home}/p").into_boxed_str());
        call
    };
    assert_eq!(at("/work/a").key(), at("/srv/ci/b").key());
    // A variable under no root stays as it is.
    let mut moved = at("/srv/ci/b");
    moved.env.insert("CC", "/opt/cc");
    assert_ne!(moved.key(), at("/srv/ci/b").key());
    // A crate with no roots is keyed by its paths.
    let mut bare = Inputs::new();
    bare.dir = "/work/a/p";
    assert_ne!(bare.key(), at("/work/a").key());
}

/// What an expansion read, written against its crate's roots, is the same in two checkouts
/// while the files are, and not once one under the root changed.
#[cfg(not(windows))] // Windows has no roots (`blaze_usercode::roots`).
#[test]
fn a_file_read_under_the_crates_package_is_keyed_by_its_place_in_it() {
    let run = RunKey { code: ContentId::of(LIBRARY, b"lib"), given: Inputs::new().key() };
    let read = |home: &str, hash: u8| {
        let env: Env =
            [("CARGO_MANIFEST_DIR".to_owned(), format!("{home}/p"))].into_iter().collect();
        let traced = Traced {
            files: vec![(PathBuf::from(format!("{home}/p/schema.json")), Some([hash; 32]))],
            ..Traced::default()
        };
        index::result_key(&run, &super::roots(&env).traced(&traced))
    };
    assert_eq!(read("/work/a", 1), read("/srv/b", 1));
    assert_ne!(read("/work/a", 1), read("/srv/b", 2));
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

fn derive<'a>(
    subtree: &'a tt::TopSubtree,
    attrs: Option<&'a tt::TopSubtree>,
    env: &'a Env,
    site: Span,
) -> Call<'a> {
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
    let key = |attrs| {
        given(&toolchain, &derive(&inputs.subtree, attrs, &inputs.env, inputs.site)).0.finish()
    };
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

/// A daemon's [`super::Keeping`], over a store of its own in `dir`, its shell passing `path`.
fn daemon(
    sim: &mut blaze_core::sim::Sim,
    dir: &str,
    path: &str,
) -> (blaze_cas::Store, super::Keeping) {
    let mut effects = Vec::new();
    let config = blaze_cas::Config {
        dir: blaze_core::FileName::new(dir).unwrap(),
        layers: Vec::new(),
        tags: 1 << 48,
        load_tags: (1 << 48) + (1 << 32),
        loader: blaze_cas::Loader::Pumped,
    };
    let mut store = blaze_cas::Store::open(config, &mut effects);
    blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    }).expect("the effects end");
    assert!(store.ready());
    let environment = super::Environment::new();
    environment.pass(vec![("PATH".into(), path.into())]).unwrap();
    let expansions = super::Expansions {
        store: store.reader(),
        toolchain: ContentId::of(LIBRARY, b"toolchain"),
        environment,
    };
    (store, super::Keeping::new(expansions).unwrap())
}

#[test]
fn two_daemons_in_one_process_each_read_their_own_store_and_shell() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(5, Faults::default());
    let (mut one, first) = daemon(&mut sim, "one", "/one/bin");
    let (_other, second) = daemon(&mut sim, "other", "/other/bin");

    let key = blaze_cas::Key(ContentId::of(LIBRARY, b"kept by the first daemon"));
    one.put(key, blaze_cas::Kind::Run, b"an expansion").unwrap();
    let guard = blaze_cas::enter();
    assert!(first.expansions.store.has(&guard, &key));
    assert!(!second.expansions.store.has(&guard, &key), "the second daemon read the first's store");
    drop(guard);

    let traced =
        Traced { env: vec![("PATH".into(), Some("/one/bin".into()))], ..Traced::default() };
    let shape = index::Shape::of(&traced).encode();
    let reported = index::VALUES.traced(&traced);
    let call = Env::default();
    assert_eq!(first.reads_now(&call, &shape), Ok(reported));
    assert_ne!(
        second.reads_now(&call, &shape),
        Ok(reported),
        "the second daemon read the first's shell"
    );

    let nothing = super::Keeps::Nothing("a test that keeps nothing");
    assert_eq!(nothing.keeping().err(), Some(super::NotKept { why: "a test that keeps nothing" }));
    let refused = nothing.reads_now(&call, &shape).unwrap_err();
    assert!(refused.contains("not kept here: a test that keeps nothing"), "{refused}");
    assert_eq!(super::Keeps::In(first).reads_now(&call, &shape), Ok(reported));
}

/// C3 (round 4): an expansion found verified across checkouts reports reads whose values
/// change once the record that retracts it is in the store, so what `hir_def::deps` filed by
/// them is made again rather than kept on a value no checkout is given any more.
#[test]
fn a_verified_expansions_reads_change_once_it_is_retracted() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(7, Faults::default());
    let (mut store, keeping) = daemon(&mut sim, "verified", "/bin");
    let run =
        RunKey { code: ContentId::of(LIBRARY, b"lib"), given: ContentId::of("test", b"call") };
    let traced = Traced { env: vec![("PATH".into(), Some("/bin".into()))], ..Traced::default() };
    let unmarked = index::result_key(&run, &traced);
    let depends = index::depends_key(&run, &unmarked);
    let shape = index::Shape::of(&traced).encode();
    let (reported_shape, reported) =
        super::verified_reads(&depends, &shape, index::VALUES.traced(&traced));
    let call = Env::default();
    assert_eq!(
        keeping.reads_now(&call, &reported_shape),
        Ok(reported),
        "unchanged while it is not retracted"
    );
    // A test that keeps nothing has no store to ask: it says so, never guesses.
    assert!(super::reads_in(&[], &call, &reported_shape).unwrap_err().contains("with the store"));

    let record = blaze_usercode::roots::verify::retraction(&run, &unmarked);
    let put = super::Put {
        keep: super::Keep::Durable { run: record.run, traced: record.traced },
        value: record.value,
        what: "retraction".to_owned(),
    };
    let published = super::publish(&mut store, vec![put]);
    assert!(published.refused.is_empty() && !published.durable.is_empty(), "{published:?}");
    assert_ne!(
        keeping.reads_now(&call, &reported_shape),
        Ok(reported),
        "a retracted expansion's reads changed"
    );
    // A plain shape is as it was.
    assert_eq!(keeping.reads_now(&call, &shape), Ok(index::VALUES.traced(&traced)));
}

/// C1 (round 6), P1 (round 8): a warm daemon's front end keeps what it was given (a tracked
/// expansion, a served image) without asking the store again, so once a record that retracts
/// a verified value is published the epoch a kept front end is lowered into at moves
/// (`Keeping::retractions_now`): the next build lowers afresh and looks the expansion up again.
/// The store has one writer, its daemon, so every record goes through its publish
/// (`Keeping::publish`), which moves the epoch once the record is in the store; a publish
/// with no record moves nothing.
#[test]
fn a_front_end_is_not_kept_past_a_retraction_its_daemon_publishes() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(8, Faults::default());
    let (mut store, keeping) = daemon(&mut sim, "retractions", "/bin");
    let traced = Traced { env: vec![("PATH".into(), Some("/bin".into()))], ..Traced::default() };
    let run =
        RunKey { code: ContentId::of(LIBRARY, b"lib"), given: ContentId::of("test", b"served") };
    let unmarked = index::result_key(&run, &traced);
    let record = blaze_usercode::roots::verify::retraction(&run, &unmarked);
    let at = keeping.retractions_now();

    let plain = super::Put {
        keep: super::Keep::Run { run, traced: traced.clone() }, // clone: a test's record
        value: b"an expansion".to_vec(),
        what: "plain".to_owned(),
    };
    let published = keeping.publish(&mut store, vec![plain]);
    assert!(published.refused.is_empty() && published.durable.is_empty(), "{published:?}");
    assert_eq!(keeping.retractions_now(), at, "a publish with no record moves nothing");

    keeping.put(super::Put::of(record, "retraction".to_owned()));
    assert_eq!(keeping.retractions_now(), at, "put, not published yet: not in the store either");
    let published = keeping.publish(&mut store, Vec::new());
    assert!(published.refused.is_empty() && !published.durable.is_empty(), "{published:?}");
    let retracted = keeping.retractions_now();
    assert_ne!(retracted, at, "retracted: a front end kept at the old epoch is not lowered into");
    let guard = blaze_cas::enter();
    assert!(keeping.expansions.store.has(&guard, &index::depends_key(&run, &unmarked)));
    drop(guard);
    assert_eq!(
        keeping.retractions_now(),
        retracted,
        "a record counts once: the next front end is kept"
    );
    // A build script's record, published by `blaze-cargo`'s runs, moves it too.
    let keeps = super::Keeps::In(keeping);
    keeps.retracted();
    assert_ne!(keeps.retractions_now(), retracted);
}

/// C1 (round 9): a front end stamped with an epoch has seen every record whose bump is at most
/// that epoch (`Retractions`). The bump comes only once the record is readable, even when the
/// record is the last put of its batch, behind others: at the moment before it, the store has
/// the record and the epoch is still the old one, so a lowering that reads the new epoch is
/// never given the retracted value.
#[test]
fn a_front_end_stamped_at_an_epoch_has_seen_every_record_counted_in_it() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(10, Faults::default());
    let (mut store, keeping) = daemon(&mut sim, "stamped", "/bin");
    let traced = Traced { env: vec![("PATH".into(), Some("/bin".into()))], ..Traced::default() };
    let run =
        RunKey { code: ContentId::of(LIBRARY, b"lib"), given: ContentId::of("test", b"stamped") };
    let unmarked = index::result_key(&run, &traced);
    let depends = index::depends_key(&run, &unmarked);
    let at = keeping.retractions_now();
    // The record is taken last, after the batch's other puts (`Keeping::publish` appends the
    // daemon's durable queue).
    keeping.put(super::Put::of(
        blaze_usercode::roots::verify::retraction(&run, &unmarked),
        "retraction".to_owned(),
    ));
    let others: Vec<super::Put> = (0..4u8)
        .map(|n| super::Put {
            keep: super::Keep::Run {
                run: RunKey { code: run.code, given: ContentId::of("test", &[n]) },
                traced: traced.clone(), // clone: a test's record
            },
            value: vec![n],
            what: format!("plain {n}"),
        })
        .collect();
    let mut checked = false;
    let published = keeping.publish_then(&mut store, others, |store, epoch| {
        assert_eq!(epoch, at, "not bumped before the record is readable");
        assert!(
            store.get(&depends).unwrap().is_some(),
            "the bump comes before its record is in the store"
        );
        let guard = blaze_cas::enter();
        assert!(
            keeping.expansions.store.has(&guard, &depends),
            "a lowering's reader would not see the record"
        );
        checked = true;
    });
    assert!(checked, "a publish with a record bumps the epoch");
    assert!(published.refused.is_empty() && !published.durable.is_empty(), "{published:?}");
    let stamped = keeping.retractions_now();
    assert_ne!(stamped, at);
    // A lowering stamped with the new epoch reads the store after it: the record is there.
    let guard = blaze_cas::enter();
    assert!(keeping.expansions.store.has(&guard, &depends));
}

/// C2 (round 10): a record's result key is readable before its shape slots are kept
/// (`index::keep_in`), so a durable put refused after that (every slot of its chain holds
/// another shape: `TooManyShapes`) has still made its record visible. The epoch moves for it
/// all the same: a front end stamped before it is not kept on the value it retracts. The
/// refusal is told `server_failed`, as every refused put is.
#[test]
fn a_record_readable_before_its_put_fails_still_moves_the_epoch() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(11, Faults::default());
    let (mut store, keeping) = daemon(&mut sim, "refused-record", "/bin");
    let traced = Traced { env: vec![("PATH".into(), Some("/bin".into()))], ..Traced::default() };
    let run = RunKey {
        code: ContentId::of(LIBRARY, b"lib"),
        given: ContentId::of("test", b"refused record"),
    };
    let unmarked = index::result_key(&run, &traced);
    let depends = index::depends_key(&run, &unmarked);
    let record = blaze_usercode::roots::verify::retraction(&run, &unmarked);
    // Every slot of the record's chain holds another shape, so its put fails past its value.
    for n in 0..index::MOST_SHAPES {
        let at = index::slot(&record.run, n);
        let other = [b"another shape ".as_slice(), &n.to_le_bytes()].concat(); // alloc: a test's slot
        store.put(at, blaze_cas::Kind::Run, &other).unwrap();
    }
    let at = keeping.retractions_now();
    keeping.put(super::Put::of(record, "retraction".to_owned()));
    let published = keeping.publish(&mut store, Vec::new());
    assert_eq!(published.refused.len(), 1, "{published:?}");
    assert!(
        store.get(&depends).unwrap().is_some(),
        "the record is readable though its put was refused"
    );
    // Readable, so waited on: its value's put landed before the shapes ran out, and a crash
    // before it is durable would forget a record every checkout already read.
    let ticket = published.durable.iter().next().expect("the record's landed put is waited on");
    assert!(published.tickets.contains(&ticket), "{published:?}");
    assert!(!store.durable(ticket), "not durable before a flush");
    assert_ne!(keeping.retractions_now(), at, "a visible record moved no epoch");
    let guard = blaze_cas::enter();
    assert!(keeping.expansions.store.has(&guard, &depends));
}

/// D1 and M2 (round 8): a job is reported done only once the record it put is durable, so the
/// record must reach the store's owner as that job's, by the job's end. A durable put is its
/// daemon's ([`super::Keeping::put`]), never in the queue any task's take drains: a task of
/// another job that took it there and was descheduled would hold it past the end of the job
/// that made it, and that job would be reported done with its record not durable. Only the
/// owner's publish takes it, and it hands back the ticket in the same call. Another daemon in
/// the process never takes it either, nor publishes it into its own store.
#[test]
fn a_job_waits_for_its_retraction_even_when_another_task_takes_puts_first() {
    use blaze_core::sim::{Faults, Sim};
    let mut sim = Sim::new(9, Faults::default());
    let (mut store, keeping) = daemon(&mut sim, "durable", "/bin");
    let (mut other_store, other) = daemon(&mut sim, "durable-other", "/bin");
    let traced = Traced { env: vec![("PATH".into(), Some("/bin".into()))], ..Traced::default() };
    let run = RunKey {
        code: ContentId::of(LIBRARY, b"lib"),
        given: ContentId::of("test", b"taken first"),
    };
    let unmarked = index::result_key(&run, &traced);
    let record = blaze_usercode::roots::verify::retraction(&run, &unmarked);
    // The maker's task puts the record on a worker; another job's task on another worker
    // takes its puts after that, as `plane::run_measured` does after each task.
    std::thread::scope(|scope| {
        scope
            .spawn(|| keeping.put(super::Put::of(record, "the record of `M`".to_owned())))
            .join()
            .unwrap();
        let taken = scope.spawn(super::take_puts).join().unwrap();
        assert!(
            taken.iter().all(|it| it.what != "the record of `M`"),
            "another task's take carried the record off: {taken:?}"
        );
    });
    assert!(super::take_puts().iter().all(|it| it.what != "the record of `M`"));
    // The other daemon's publish neither takes it nor puts it in its store.
    let published = other.publish(&mut other_store, Vec::new());
    assert!(published.durable.is_empty(), "{published:?}");
    let depends = index::depends_key(&run, &unmarked);
    assert!(other_store.get(&depends).unwrap().is_none(), "another daemon published the record");
    // The owner's next publish (the maker's `plane_finished` at the latest) has it, with its
    // ticket, which every job not ended waits for.
    let published = keeping.publish(&mut store, Vec::new());
    assert!(published.refused.is_empty(), "{published:?}");
    assert!(!published.durable.is_empty(), "the job's publish has the record's ticket");
    assert!(store.get(&depends).unwrap().is_some());
    assert!(keeping.publish(&mut store, Vec::new()).durable.is_empty(), "taken once");
}

/// A durable put through a thread's puts is a bug, a panic naming it, never a put the job
/// would not wait for.
#[test]
#[should_panic(expected = "a durable put goes through its daemon's `Keeping::put`")]
fn a_durable_put_through_a_threads_puts_is_refused() {
    let run =
        RunKey { code: ContentId::of(LIBRARY, b"lib"), given: ContentId::of("test", b"refused") };
    let unmarked = blaze_cas::Key(ContentId::of("test", b"unmarked"));
    super::put(super::Put::of(
        blaze_usercode::roots::verify::retraction(&run, &unmarked),
        "retraction".to_owned(),
    ));
}
