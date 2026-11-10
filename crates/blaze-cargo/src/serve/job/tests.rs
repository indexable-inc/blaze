use blaze_ids::Idx;
use blaze_vfs::View;

use super::{
    super::unit::Real,
    slab::{Key as SlabKey, Table, Tagged},
    *,
};

fn targets() -> Targets {
    Targets {
        packages: vec!["a".into(), "b".into()],
        profile: "release".into(),
        target: Some("wasm32-unknown-unknown".into()),
        features: vec!["x".into(), "y".into()],
        all_features: false,
        no_default_features: true,
        cargo: None,
    }
}

fn setup(runs: u64, runs_end: u64) -> Setup {
    Setup {
        views: "/v".into(),
        std: "/s/std".into(),
        builds: "/s/build".into(),
        sources: "/s/sources".into(),
        store: "/s/v2/store".into(),
        host: "h".into(),
        env: Vec::new(),
        color: false,
        tags: 6 << 32,
        runs,
        runs_end,
    }
}

fn opened(root: u8) -> Opened {
    Opened {
        view: View { root: ContentId([root; 32]) },
        files: Idx::from_raw(0),
        queries: Idx::from_raw(0),
    }
}

/// A job of nothing but its views, as [`Jobs::begin`] puts one in.
fn job(views: Vec<Opened>) -> Job<Real> {
    Job {
        work: Work::Snapshot,
        views,
        ready: Vec::new(),
        request: None,
        idle: false,
        closed: false,
        finished: true,
        cancelled: Arc::new(AtomicBool::new(false)),
        stage: Stage::Over,
        value: None,
        updating: HashSet::new_in(std::alloc::Global),
        said: HashSet::new_in(std::alloc::Global),
        queries: Vec::new(),
        built: None,
        borrowed: Vec::new(),
        placing: 0,
        placed: blaze_cas::PlacedBytes::default(),
        worked: None,
        busy: HashMap::new_in(std::alloc::Global),
        held: Vec::new(),
        look: 0,
        awaiting: None,
        applied: 0,
        applying: None,
        shared: false,
        runs: 0,
        durable: blaze_cas::Tickets::default(),
        storing: None,
        stored: blaze_cas::Tickets::default(),
        stored_of: None,
        durable_since: None,
        over: None,
        stats: RequestStats::default(),
        fresh: 0,
        compiled: 0,
        served: Vec::new(),
        links: Vec::new(),
        linked: Vec::new(),
        misses: Vec::new(),
        test_runs: Vec::new(),
        opened_cpu: None,
        opened_ns: 0,
        opened_unix_ns: None,
        leases: Arc::new(blaze_core::lock::Lock::new("serve.job.leases", Leases::default())),
        paused: blaze_graph::pressure::Paused::default(),
        began: std::time::Duration::ZERO,
        doing_said: None,
        missed: 0,
        durable_wait: None,
        look_wait: None,
        placed_any: false,
        replayed: false,
    }
}

/// A build's row in the history is its plane request's number, the command line it was asked
/// as and the checkout it ran in; an agent's request without one names its work and its view.
#[test]
fn a_builds_history_row_names_its_request_its_command_and_its_workspace() {
    let mut agents = job(vec![opened(4)]);
    agents.work = Work::Tables;
    agents.opening();
    let stats = RequestStats { ran: 3, cached: 5, joined: 1, ..RequestStats::default() };
    let row = agents.row(RequestId(41), Some(stats), blaze_profile::history::Outcome::Ok);
    assert_eq!(row.build_id, 41);
    assert_eq!(row.command, "tables");
    assert_eq!(row.workspace, format!("view {}", ContentId([4; 32])));
    assert_eq!((row.hits, row.misses, row.joined), (Some(5), Some(3), Some(1)));
    assert!(row.dur_ns.is_some_and(|it| it >= 0) && row.started_unix_ns.is_some(), "{row:?}");
    let stalled = agents.row(RequestId(42), None, blaze_profile::history::Outcome::Stalled);
    assert_eq!((stalled.outcome, stalled.hits), (blaze_profile::history::Outcome::Stalled, None));
    assert_eq!(command_line(&Work::Snapshot), "snapshot");
}

/// A request patched in place is one whose every link was spliced or left unchanged: a first
/// link, or one that laid out a kept image whole again, makes it placed.
#[test]
fn work_is_placed_whole_when_any_link_was() {
    use crate::serve::work::Full;
    let link = |how| ("/w/target/debug/app".to_owned(), how);
    assert_eq!(link_kind(&[]), LinkKind::Patched);
    assert_eq!(link_kind(&[link(Linking::Spliced), link(Linking::Spliced)]), LinkKind::Patched);
    assert_eq!(link_kind(&[link(Linking::Spliced), link(Linking::First)]), LinkKind::Placed);
    assert_eq!(link_kind(&[link(Linking::Unchanged), link(Linking::Spliced)]), LinkKind::Patched);
    assert_eq!(link_kind(&[link(Linking::Full(Full::NotIncremental))]), LinkKind::Placed);
    assert_eq!(link_kind(&[link(Linking::Full(Full::NoThunk))]), LinkKind::Placed);

    use blaze_api::LinkHow;
    assert_eq!(Linking::Spliced.how(), LinkHow::Spliced);
    assert_eq!(Linking::Unchanged.how(), LinkHow::Unchanged);
    assert_eq!(Linking::First.how(), LinkHow::First);
    assert_eq!(
        Linking::Full(Full::NotIncremental).how(),
        LinkHow::Full { why: vec!["not incremental".to_owned()] }
    );
    assert_eq!(
        Linking::Full(Full::NoThunk).how(),
        LinkHow::Full { why: vec!["no thunk in reach".to_owned()] }
    );
}

#[test]
fn an_agents_targets_are_cargos_flags_at_the_root() {
    let env =
        vec![("HOME".to_owned(), "/home/u".to_owned()), ("PATH".to_owned(), "/bin".to_owned())];
    let work = Work::Build(targets());
    let made = invocation(&work, targets_of(&work), "/w", &env);
    assert_eq!(made.command, Command::Build);
    let args: Vec<&str> = made.args.iter().map(String::as_str).collect();
    assert_eq!(
        args,
        [
            "-p",
            "a",
            "-p",
            "b",
            "--profile",
            "release",
            "--target",
            "wasm32-unknown-unknown",
            "-F",
            "x,y",
            "--no-default-features"
        ]
    );
    assert_eq!((made.root.as_str(), made.cwd.as_str()), ("/w", "/w"));
    assert_eq!(made.env, env);
    assert!(made.files.is_empty());
}

#[test]
fn cargos_own_invocation_is_kept_whole() {
    let mut targets = targets();
    targets.cargo = Some(blaze_api::Cargo {
        command: "test".into(),
        args: vec!["--workspace".into()],
        root: "/w".into(),
        cwd: "/w/a".into(),
        env: vec![("RUSTFLAGS".into(), "-Dwarnings".into())],
        files: Vec::new(),
        color: false,
        width: None,
        verbose: 0,
        refused: Some(16),
    });
    let work = Work::Test {
        targets: targets.clone(),
        filter: Vec::new(),
        all: false,
        args: Vec::new(),
        ub: None,
    };
    let made = invocation(&work, Some(&targets), "/elsewhere", &[]);
    assert_eq!(made.command, Command::Test);
    assert_eq!(made.args, vec!["--workspace".to_owned()]);
    assert_eq!(made.cwd, "/w/a");
    assert_eq!(made.env, vec![("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())]);
}

#[test]
fn diagnostics_are_short_as_cargos_message_format_says() {
    let short = |args: &[&str]| short(&args.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>());
    assert_eq!(short(&[]), Ok(false));
    assert_eq!(short(&["--message-format=short"]), Ok(true));
    assert_eq!(short(&["--message-format", "short", "-p", "a"]), Ok(true));
    assert_eq!(short(&["--message-format=json,json-diagnostic-short"]), Ok(true));
    assert_eq!(short(&["--message-format=json-diagnostic-rendered-ansi"]), Ok(false));
    assert_eq!(short(&["--message-format=human"]), Ok(false));
    assert!(short(&["--message-format=short", "--message-format=human"]).is_err());
    assert!(short(&["--message-format=terse"]).is_err());
}

#[test]
fn tests_under_the_evaluator_check_the_units_and_build_none() {
    let ub = Some(blaze_api::Ub { seed: Some(7), schedules: None });
    let work =
        Work::Test { targets: targets(), filter: Vec::new(), all: true, args: Vec::new(), ub };
    assert_eq!(command_of(&work), Command::Test);
    assert_eq!(goal_of(&work), Goal::Check);
    let compiled = Work::Test {
        targets: targets(),
        filter: Vec::new(),
        all: true,
        args: Vec::new(),
        ub: None,
    };
    assert_eq!(goal_of(&compiled), Goal::Test);
}

#[test]
fn a_build_ahead_compiles_tests_and_an_edit_only_checks() {
    let ahead = Work::Ahead { targets: targets(), filter: vec!["parse".into()] };
    assert_eq!((command_of(&ahead), goal_of(&ahead)), (Command::Test, Goal::Test));
    assert!(targets_of(&ahead).is_some());
    let edit = Work::Edit(Vec::new());
    assert_eq!((command_of(&edit), goal_of(&edit)), (Command::Check, Goal::Check));
    assert!(targets_of(&edit).is_none());
    assert!(reuses_plans(&edit) && reuses_plans(&ahead));
}

#[test]
fn a_pack_checks_the_targets_it_names() {
    let pack = Work::Pack { targets: targets(), program: "app".into() };
    assert_eq!((command_of(&pack), goal_of(&pack)), (Command::Check, Goal::Check));
    assert_eq!(targets_of(&pack), Some(&targets()));
    assert!(reuses_plans(&pack));
}

#[test]
fn a_fetchs_key_is_its_source() {
    let fetch = |at: &str, sha: &str| Fetch {
        what: "serde v1.0.0".into(),
        url: "https://static.crates.io/crates/serde/1.0.0/download".into(),
        kind: FetchKind::Crate { sha256: sha.into() },
        at: at.into(),
    };
    assert_eq!(
        fetch_key(&fetch("/r/serde-1.0.0", "aa")),
        fetch_key(&fetch("/r/serde-1.0.0", "aa"))
    );
    assert_ne!(
        fetch_key(&fetch("/r/serde-1.0.0", "aa")),
        fetch_key(&fetch("/r/serde-1.0.0", "bb"))
    );
    assert_ne!(
        fetch_key(&fetch("/r/serde-1.0.0", "aa")),
        fetch_key(&fetch("/s/serde-1.0.0", "aa"))
    );
}

#[test]
fn only_a_checkouts_last_plan_and_front_end_are_kept() {
    assert!(lasting("/v", "/home/u/w"));
    assert!(lasting("/v", "/very/w"), "a directory beside the views' is a checkout's");
    assert!(!lasting("/v", "/v/0123abcd"), "an agent's view is placed once");
}

#[test]
fn a_slab_reuses_a_slot_under_a_new_generation_and_refuses_the_stale_id() {
    let mut slab: Tagged<&str> = Tagged::new(4, 8, MemTag::Other).unwrap();
    let (a, a_bits) = slab.insert("a").unwrap();
    let (b, b_bits) = slab.insert("b").unwrap();
    assert_eq!((a.index(), b.index()), (0, 1));
    assert_eq!((slab.find(a_bits), slab.find(b_bits)), (Some(a), Some(b)));
    assert_eq!((slab.get(a), slab.get(b)), (Some(&"a"), Some(&"b")));
    assert_eq!(slab.remove(a), Some("a"));
    assert_eq!((slab.get(a), slab.find(a_bits)), (None, None));
    assert_eq!(slab.remove(a), None, "a removed id removes nothing again");
    let (c, c_bits) = slab.insert("c").unwrap();
    assert_eq!(c.index(), a.index(), "the slot last freed is reused first");
    assert_ne!((c, c_bits), (a, a_bits), "under a new generation");
    assert_eq!(slab.get(a), None, "the stale id finds nothing in its slot's new value");
    assert_eq!(slab.get_mut(a), None);
    assert_eq!(slab.find(a_bits), None, "nor do its bits");
    assert_eq!(slab.find(c_bits), Some(c));
    *slab.get_mut(c).unwrap() = "C";
    let all: Vec<(u32, &str)> = slab.iter_mut().map(|(handle, it)| (handle.index(), *it)).collect();
    assert_eq!(all, [(0, "C"), (1, "b")]);
}

#[test]
fn a_full_slab_refuses_the_value() {
    let mut slab: Tagged<u32> = Tagged::new(2, 4, MemTag::Other).unwrap();
    let first = slab.insert(0).unwrap();
    for it in 1..4 {
        assert!(slab.insert(it).is_ok());
    }
    assert!(slab.insert(9).is_err(), "four slots hold four values");
    assert_eq!(slab.remove(first.0), Some(0));
    let again = slab.insert(9).unwrap();
    assert_eq!(again.0.index(), first.0.index());
    assert_eq!(slab.find(first.1), None);
}

#[test]
fn a_slab_id_packs_into_its_bits_and_back() {
    let mut slab: Tagged<u8> =
        Tagged::new(OWN_SLOT_BITS, OWN_GENERATION_BITS, MemTag::Other).unwrap();
    let (mut handle, mut packed) = slab.insert(1).unwrap();
    let first = packed;
    for reuse in 1..(1u32 << (OWN_GENERATION_BITS - 1)) + 3 {
        slab.remove(handle);
        (handle, packed) = slab.insert(1).unwrap();
        assert!(packed < 1 << 32, "a tag of the loop's own stays in its range");
        assert_eq!(slab.find(packed), Some(handle));
        if reuse < 1 << (OWN_GENERATION_BITS - 1) {
            assert_ne!(packed, first, "no tag comes back before its generation bits wrap");
        }
    }
    assert_eq!(slab.find(1 << 32), None, "bits past the generation's are no id");
    assert_eq!(slab.find(1), None, "an empty slot is no id");
}

#[test]
fn a_table_holds_ids_handed_out_elsewhere() {
    let mut table: Table<&str> = Table::new();
    assert!(table.is_empty());
    let first = SlabKey { slot: 5, generation: 0 };
    assert_eq!(table.put(first, "a"), Ok(()));
    assert_eq!(table.put(first, "b"), Err("b"), "a slot held is put nothing else");
    assert_eq!(table.get(first), Some(&"a"));
    assert_eq!(table.get(SlabKey { slot: 4, generation: 0 }), None);
    assert_eq!(table.get(SlabKey { slot: 9, generation: 0 }), None);
    assert_eq!(table.remove(first), Some("a"));
    assert!(table.is_empty());
    let next = SlabKey { slot: 5, generation: 1 };
    assert_eq!(table.put(next, "c"), Ok(()));
    assert_eq!(table.get(first), None, "the slot's last job's id finds nothing");
    assert_eq!(table.remove(first), None);
    assert_eq!(table.remove(next), Some("c"));
    assert_eq!(table.put(first, "d"), Err("d"), "an id older than its slot's last is stale");
}

#[test]
fn a_node_finds_its_batchs_job_whatever_the_order_batches_came_in() {
    let mut batches = Batches::new();
    let (a, b, c) = (
        JobId { slot: 0, generation: 0 },
        JobId { slot: 1, generation: 0 },
        JobId { slot: 0, generation: 1 },
    );
    batches.insert(10, 5, a);
    batches.insert(15, 1, b);
    batches.insert(u32::MAX - 1, 2, a);
    batches.insert(0, 3, c);
    assert_eq!(batches.firsts, [0, 10, 15, u32::MAX - 1]);
    let found = |node: u32| batches.job_of(NodeId(node));
    assert_eq!([found(0), found(2), found(3), found(9)], [Some(c), Some(c), None, None]);
    assert_eq!([found(10), found(14), found(15), found(16)], [Some(a), Some(a), Some(b), None]);
    assert_eq!(found(u32::MAX), Some(a));
    batches.remove(a);
    assert_eq!(batches.firsts, [0, 15]);
    assert_eq!((batches.job_of(NodeId(12)), batches.job_of(NodeId(15))), (None, Some(b)));
}

#[test]
fn a_tag_of_the_loops_own_names_its_effect_once() {
    let mut jobs = Jobs::<Real>::new(setup(0, 0)).unwrap();
    let job = JobId { slot: 3, generation: 7 };
    let key = blaze_cas::Key(ContentId([1; 32]));
    let tag = jobs.own_tag(Own::Record { job, path: 0, key }).unwrap();
    let other = jobs.own_tag(Own::Write { job, path: 1 }).unwrap();
    assert_ne!(tag, other);
    assert!(jobs.owns(tag) && jobs.owns(other));
    let found = jobs.own.find(tag.0 - jobs.setup.tags).unwrap();
    assert!(matches!(jobs.own.remove(found), Some(Own::Record { path: 0, .. })));
    assert!(jobs.own.remove(found).is_none(), "a second completion of a tag finds nothing");
    assert!(jobs.own.find(tag.0 - jobs.setup.tags).is_none());
    let again = jobs.own_tag(Own::Look { job, path: 2, key, executable: false }).unwrap();
    assert_ne!(again, tag, "the freed slot comes back under a new generation");
    assert!(jobs.own.find(tag.0 - jobs.setup.tags).is_none(), "the old tag names nothing in it");
    assert!(jobs.own.find(again.0 - jobs.setup.tags).is_some());
}

#[test]
fn a_run_of_user_code_is_found_by_any_of_its_tags_until_it_ends() {
    let (runs, runs_end) = (1 << 50, 1 << 62);
    let mut jobs = Jobs::<Real>::new(setup(runs, runs_end)).unwrap();
    let id = JobId { slot: 0, generation: 0 };
    let (run, tags) = jobs.run_tags(id, Some(NodeId(4))).unwrap();
    assert!(tags >= runs && (tags - runs) % RUN_TAGS == 0, "a run's tags start a range of its own");
    assert_eq!(jobs.run_of(tags), Some(run));
    assert_eq!(jobs.run_of(tags + (1 << 32) - 1), Some(run));
    let (second, second_tags) = jobs.run_tags(id, None).unwrap();
    assert_eq!(second_tags, tags + RUN_TAGS, "the next slot, the same generation");
    assert_eq!(jobs.run_of(second_tags + 9), Some(second));
    jobs.run_over(run);
    assert_eq!(jobs.run_of(tags), None, "an ended run's tags find nothing");
    let (third, third_tags) = jobs.run_tags(id, None).unwrap();
    assert_eq!(third.index(), run.index());
    assert_ne!(third_tags, tags, "a run in a reused slot has tags of its own");
    assert!(third_tags < runs_end && jobs.owns(Tag(third_tags)));
    assert_eq!(jobs.run_of(tags), None);
    assert_eq!(jobs.run_of(third_tags), Some(third));
}

#[test]
fn a_daemon_with_no_tags_for_runs_refuses_one() {
    let mut jobs = Jobs::<Real>::new(setup(0, 0)).unwrap();
    assert!(jobs.run_tags(JobId { slot: 0, generation: 0 }, None).is_err());
}

#[test]
fn jobs_are_walked_in_the_order_they_started_whatever_their_slots() {
    let mut jobs = Jobs::<Real>::new(setup(0, 0)).unwrap();
    let slots = [9u32, 1, 300, 2, 0, 77];
    for (root, slot) in (0u8..).zip(slots) {
        let id = JobId { slot, generation: u32::from(root) };
        assert!(jobs.jobs.put(key_of(id), job(vec![opened(root)])).is_ok());
        jobs.order.push(id);
    }
    let roots = |jobs: &Jobs<Real>| jobs.roots().into_iter().map(|it| it.0[0]).collect::<Vec<u8>>();
    assert_eq!(roots(&jobs), [0, 1, 2, 3, 4, 5]);
    let gone = JobId { slot: 300, generation: 2 };
    assert!(jobs.jobs.remove(key_of(gone)).is_some());
    jobs.order.retain(|it| *it != gone);
    assert_eq!(roots(&jobs), [0, 1, 3, 4, 5]);
    let stale = JobId { slot: 9, generation: 1 };
    assert!(jobs.jobs.get(key_of(stale)).is_none(), "an id of another generation finds nothing");
}

/// A path is not all there while any effect placing it is in flight (a placement, then its
/// mode, then the look at what was placed): what a build script's run waits on before it runs
/// its program.
#[test]
fn a_path_is_placed_once_every_effect_placing_it_has_ended() {
    let mut jobs = Jobs::<Real>::new(setup(0, 0)).unwrap();
    let id = JobId { slot: 0, generation: 0 };
    assert!(jobs.jobs.put(key_of(id), job(Vec::new())).is_ok());
    jobs.begin_placing(id, 4);
    jobs.begin_placing(id, 4);
    jobs.begin_placing(id, 5);
    let busy = |jobs: &Jobs<Real>| {
        let job = jobs.jobs.get(key_of(id)).unwrap();
        let mut busy: Vec<u32> = job.busy.keys().copied().collect();
        busy.sort_unstable();
        (job.placing, busy)
    };
    assert_eq!(busy(&jobs), (3, vec![4, 5]));
    jobs.end_placing(id, 4);
    assert_eq!(busy(&jobs), (2, vec![4, 5]), "one of path 4's two is in flight");
    jobs.end_placing(id, 5);
    jobs.end_placing(id, 4);
    assert_eq!(busy(&jobs), (0, vec![]));
    let gone = JobId { slot: 1, generation: 0 };
    jobs.begin_placing(gone, 4);
    assert!(jobs.end_placing(gone, 4).is_none(), "a job no longer there counts nothing");
}

#[test]
fn an_output_path_is_interned_once_and_found_among_its_units_outputs() {
    let mut paths = Paths::<Real>::new(u64::MAX);
    let a = paths.intern("/w/target/debug/a");
    let b = paths.intern("/w/target/debug/b");
    assert_eq!(paths.intern("/w/target/debug/a"), a);
    assert_ne!(a, b);
    assert_eq!(
        paths.files[a as usize].as_ref().map(ToString::to_string).as_deref(),
        Some("w/target/debug/a")
    );
    assert_eq!(paths.find(&[b, a], "/w/target/debug/a"), a);
    let c = paths.find(&[b], "/w/target/debug/c");
    assert_eq!(&*paths.names[c as usize], "/w/target/debug/c");
    assert!(paths.take_images(&[a, b, c]).is_empty());
}

/// memcap3 review K1: a link takes its images before the eviction that the images of compiles
/// that gave way (put back first) may call for, so the budget never lets go of the image the
/// link is about to patch. Path `a` is the least recently linked, so an eviction that ran
/// before the take would let it go first and the link would find none kept.
#[test]
fn a_link_takes_its_kept_image_before_the_budget_evicts() {
    use crate::serve::tests::{Fake, FakeImage};
    let image = |path: &str, held: u64| Image::<Fake> {
        path: path.into(),
        kept: FakeImage { held, live: false },
    };
    let mut paths = Paths::<Fake>::new(u64::MAX);
    paths.keep_one(&[], image("/w/target/debug/a", 60), true);
    paths.keep_one(&[], image("/w/target/debug/b", 30), true);
    paths.keep_one(&[], image("/w/target/debug/c", 40), true);
    let [a, b, c] = ["a", "b", "c"].map(|it| paths.intern(&format!("/w/target/debug/{it}")));
    // A compile takes `c` and gives way: its image comes back through `back`, as it was taken.
    let taken = paths.take_images_under(&[c], 50);
    assert_eq!(taken.len(), 1);
    for image in taken {
        paths.back.push(image);
    }
    assert_eq!(paths.weights.held(), 90);
    // The link of `a`: `c` is kept again (130 held), `a` taken (70), then past 50 `b` goes,
    // the least recently linked left.
    let got = paths.take_images_under(&[a], 50);
    let got: Vec<&str> = got.iter().map(|it| it.path.as_str()).collect(); // alloc: a test's one
    assert_eq!(got, ["/w/target/debug/a"], "the link gets the image it patches");
    assert!(
        paths.images[b as usize].is_none(),
        "past the budget, the least recently linked image left is let go"
    );
    assert!(paths.images[c as usize].is_some(), "the rest fit");
    assert_eq!((paths.kept, paths.weights.held()), (1, 40));
}

/// A job that ends while its request is open cancels it when not ok and closes it when ok
/// (once); one that ends after the plane finished its request, before the job read so, does
/// neither, where the plane would refuse ("request 1 is not open").
#[test]
fn a_job_ending_after_the_plane_finished_its_request_neither_cancels_nor_closes_it() {
    assert_eq!(at_its_end(true, false, false), (true, false));
    assert_eq!(at_its_end(true, false, true), (true, false));
    assert_eq!(at_its_end(true, true, false), (false, true));
    assert_eq!(at_its_end(true, true, true), (false, false));
    for (ok, closed) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_eq!(at_its_end(false, ok, closed), (false, false), "ok {ok}, closed {closed}");
    }
}

#[test]
fn a_builds_crate_graph_has_an_edge_for_each_table_a_dependency_is_in() {
    use crate::serve::tests::{of_graph, unit};
    let root = "/w";
    let app = format!("path+file://{root}#app@0.1.0");
    let dep = "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0".to_owned();
    let mut tested = unit(&app, "lib", UnitMode::Test, &[0, 3]);
    tested.dependencies[0].dev = true;
    let graph = blaze_units::schema::UnitGraph {
        version: 1,
        units: vec![
            unit(&dep, "lib", UnitMode::Build, &[]),
            unit(&app, "custom-build", UnitMode::Build, &[0]),
            unit(&app, "custom-build", UnitMode::RunCustomBuild, &[1]),
            unit(&app, "lib", UnitMode::Build, &[0, 2]),
            tested,
        ],
        roots: vec![4],
    };
    let units = of_graph(root, String::new(), graph);
    let rows = crate_deps(7, &units, &[0, 1, 2, 3, 4]);
    let edges: Vec<_> =
        rows.iter().map(|it| (it.build_id, it.krate.as_str(), it.dep.as_str(), it.kind)).collect();
    // The script's run, and the test's edge to its own library, are no table's.
    assert_eq!(
        edges,
        [
            (7, "app", "dep", DepKind::Build),
            (7, "app", "dep", DepKind::Dev),
            (7, "app", "dep", DepKind::Normal)
        ]
    );
    assert!(crate_deps(7, &units, &[0]).is_empty(), "a build of the dependency alone has no edge");
}

#[test]
fn a_diagnostic_of_no_unit_is_said_once_a_job_and_a_units_each_time() {
    use crate::serve::tests::{of_graph, unit};
    let root = "/w";
    let app = format!("path+file://{root}#app@0.1.0");
    let graph = blaze_units::schema::UnitGraph {
        version: 1,
        units: vec![
            unit(&app, "custom-build", UnitMode::Build, &[]),
            unit(&app, "lib", UnitMode::Build, &[]),
        ],
        roots: vec![1],
    };
    let units = of_graph(root, String::new(), graph);
    let of = |unit: Option<UnitOf>, rendered: &str| blaze_api::Diagnostic {
        level: blaze_api::Level::Error,
        code: None,
        unit,
        facts: Vec::new(),
        spans: Vec::new(),
        message: rendered.to_owned(),
        children: Vec::new(),
        rendered: rendered.to_owned(),
        explanation: None,
    };
    let report = "error: the front end fails on `std::f` in the toolchain's `std`, ...\n";
    let line =
        "error: `app v0.1.0 (build script)` reaches 1 function blaze could not compile, ...\n";
    let mut said = HashSet::new_in(std::alloc::Global);
    // The first unit that reaches the toolchain's failed function carries the job's report
    // and its own error; the second carries the same two.
    for at in [0, 1] {
        let report_said = said_once(&mut said, &of(None, report));
        assert_eq!(report_said, at == 0, "the job's report is said by the first unit only");
        assert!(
            said_once(&mut said, &of(unit_of(&units, at), line)),
            "unit {at}'s own error is always said"
        );
    }
    assert!(
        said_once(&mut said, &of(None, "error: the front end fails on `std::g` ...\n")),
        "another report is said"
    );
}

/// What a test's watches say of everything outside the views, and of each directory.
struct Says {
    quiet: bool,
    moved: &'static [&'static str],
}

impl Vouches for Says {
    fn quiet_since(&self, _: u64) -> bool {
        self.quiet
    }

    fn unchanged_since(&self, dir: &std::path::Path, _: u64) -> bool {
        !self.moved.iter().any(|it| std::path::Path::new(it) == dir)
    }
}

/// A state a build with `BLAZE_NOOP=replay` is asked in after a kept answer: what happened
/// since, as the daemon sees it, and whether a walk would still give that answer.
struct NoopState {
    what: &'static str,
    same_key: bool,
    alone: bool,
    says: Says,
    walk_says_the_same: bool,
}

const NOOP_STATES: [NoopState; 6] = [
    NoopState {
        what: "nothing changed",
        same_key: true,
        alone: true,
        says: Says { quiet: true, moved: &[] },
        walk_says_the_same: true,
    },
    // The key is the view's root and the whole config: the request's environment is in it.
    NoopState {
        what: "a source file, a flag or a variable of the request changed",
        same_key: false,
        alone: true,
        says: Says { quiet: true, moved: &[] },
        walk_says_the_same: false,
    },
    NoopState {
        what: "another job is under way",
        same_key: true,
        alone: false,
        says: Says { quiet: true, moved: &[] },
        walk_says_the_same: false,
    },
    NoopState {
        what: "a file outside the view that a build script traced changed",
        same_key: true,
        alone: true,
        says: Says { quiet: false, moved: &[] },
        walk_says_the_same: false,
    },
    // A change under a directory of outputs is a change outside the views too, but the
    // outputs are asked of on their own: a directory not watched since before the walk
    // vouches for nothing though nothing was heard.
    NoopState {
        what: "an output was deleted",
        same_key: true,
        alone: true,
        says: Says { quiet: true, moved: &["/b/debug"] },
        walk_says_the_same: false,
    },
    NoopState {
        what: "`blaze clean` removed the build directory",
        same_key: true,
        alone: true,
        says: Says { quiet: false, moved: &["/b/debug", "/b/release"] },
        walk_says_the_same: false,
    },
];

/// The oracle of a no-op's replay: each state of [`NOOP_STATES`] where
/// [`LastAnswer::replayable`] and the walk disagree, with `plant` planted.
fn noop_findings(plant: Option<Plant>) -> Vec<&'static str> {
    let key = ContentId([1; 32]);
    let answer = LastAnswer {
        key,
        generation: 7,
        dirs: vec![std::path::PathBuf::from("/b/debug"), std::path::PathBuf::from("/b/release")]
            .into(),
        responses: Vec::new().into(),
        worked: None,
    };
    PLANTED.set(plant);
    let found = NOOP_STATES
        .iter()
        .filter(|it| {
            let asked = if it.same_key { key } else { ContentId([2; 32]) };
            answer.replayable(asked, it.alone, &it.says).is_ok() != it.walk_says_the_same
        })
        .map(|it| it.what)
        .collect();
    PLANTED.set(None);
    found
}

/// The invariant: a kept answer is replayed only in a state where the walk would say the
/// same, and in the state where nothing changed it is replayed.
#[test]
fn a_noop_is_replayed_only_while_a_walk_would_say_the_same() {
    assert_eq!(noop_findings(None), Vec::<&str>::new());
}

#[test]
fn the_oracle_catches_a_noop_that_ignores_a_deleted_output() {
    assert_eq!(noop_findings(Some(Plant::IgnoresOutputs)), ["an output was deleted"]);
}

#[test]
fn the_oracle_catches_a_noop_that_ignores_a_traced_file_outside_the_view() {
    assert_eq!(
        noop_findings(Some(Plant::IgnoresOutside)),
        ["a file outside the view that a build script traced changed"]
    );
}

#[test]
fn the_oracle_catches_a_noop_that_ignores_the_view_and_the_config() {
    assert_eq!(
        noop_findings(Some(Plant::IgnoresKey)),
        ["a source file, a flag or a variable of the request changed"]
    );
}

#[test]
fn the_oracle_catches_a_noop_that_ignores_another_job() {
    assert_eq!(noop_findings(Some(Plant::IgnoresOthers)), ["another job is under way"]);
}

/// The directories asked of are each output's first directory under its build dir, never the
/// build dir itself (every build writes its record there); an output no such directory holds
/// leaves the answer unkept.
#[test]
fn a_builds_outputs_are_vouched_for_by_the_directories_under_its_build_dirs() {
    let dirs = |outputs: &[&'static str]| output_dirs_of(["/b", "/t"], outputs.iter().copied());
    assert_eq!(
        dirs(&["/b/debug/deps/liba.rlib", "/b/debug/app", "/t/x86_64/debug/app", "/b/debug/b"]),
        Some(vec!["/b/debug".into(), "/t/x86_64".into()])
    );
    assert_eq!(dirs(&[]), Some(Vec::new()));
    assert_eq!(dirs(&["/b/app"]), None, "an output right in the build dir");
    assert_eq!(dirs(&["/elsewhere/debug/app"]), None, "an output under no build dir");
}

#[test]
fn the_noop_variable_asks_for_a_replay_or_a_walk_and_refuses_anything_else() {
    let env = |value: &str| vec![(NOOP.to_owned(), value.to_owned())];
    assert_eq!(noop_asked(&[]).ok(), Some(false));
    assert_eq!(noop_asked(&env("walk")).ok(), Some(false));
    assert_eq!(noop_asked(&env("replay")).ok(), Some(true));
    let refused = noop_asked(&env("yes")).unwrap_err();
    assert!(refused.what.contains("BLAZE_NOOP") && refused.what.contains("`yes`"), "{}", refused.what);
}
