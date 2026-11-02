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
    Setup { views: "/v".into(), std: "/s/std".into(), host: "h".into(), env: Vec::new(), color: false, tags: 6 << 32, runs, runs_end }
}

fn opened(root: u8) -> Opened {
    Opened { view: View { root: ContentId([root; 32]) }, files: Idx::from_raw(0), queries: Idx::from_raw(0) }
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
        updating: HashSet::default(),
        queries: Vec::new(),
        built: None,
        borrowed: Vec::new(),
        placing: 0,
        busy: HashMap::default(),
        held: Vec::new(),
        look: 0,
        awaiting: None,
        shared: false,
        runs: 0,
        over: None,
        stats: RequestStats::default(),
        fresh: 0,
        compiled: 0,
        served: Vec::new(),
        links: Vec::new(),
        profiled: false,
        opened_ns: 0,
        opened_unix_ns: None,
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

/// A request patched in place is one whose every link was spliced: a first link, or one that
/// laid out a kept image whole again, makes it placed.
#[test]
fn work_is_placed_whole_when_any_link_was() {
    use crate::serve::work::Full;
    let link = |how| ("/w/target/debug/app".to_owned(), how);
    assert_eq!(link_kind(&[]), LinkKind::Patched);
    assert_eq!(link_kind(&[link(Linking::Spliced), link(Linking::Spliced)]), LinkKind::Patched);
    assert_eq!(link_kind(&[link(Linking::Spliced), link(Linking::First)]), LinkKind::Placed);
    assert_eq!(link_kind(&[link(Linking::Full(Full::NotIncremental))]), LinkKind::Placed);

    use blaze_api::LinkHow;
    assert_eq!(Linking::Spliced.how(), LinkHow::Spliced);
    assert_eq!(Linking::First.how(), LinkHow::First);
    assert_eq!(Linking::Full(Full::NotIncremental).how(), LinkHow::Full { why: vec!["not incremental".to_owned()] });
}

#[test]
fn an_agents_targets_are_cargos_flags_at_the_root() {
    let env = vec![("HOME".to_owned(), "/home/u".to_owned()), ("PATH".to_owned(), "/bin".to_owned())];
    let work = Work::Build(targets());
    let made = invocation(&work, targets_of(&work), "/w", &env);
    assert_eq!(made.command, Command::Build);
    let args: Vec<&str> = made.args.iter().map(String::as_str).collect();
    assert_eq!(
        args,
        [
            "-p", "a", "-p", "b", "--profile", "release", "--target", "wasm32-unknown-unknown", "-F", "x,y",
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
    let work = Work::Test { targets: targets.clone(), filter: Vec::new(), all: false, args: Vec::new(), ub: None };
    let made = invocation(&work, Some(&targets), "/elsewhere", &[]);
    assert_eq!(made.command, Command::Test);
    assert_eq!(made.args, vec!["--workspace".to_owned()]);
    assert_eq!(made.cwd, "/w/a");
    assert_eq!(made.env, vec![("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned())]);
}

/// A job is profiled when its cargo command line's environment names a directory in
/// `BLAZE_PROFILE`; an empty value, an agent's request and work of no targets are not.
#[test]
fn a_job_is_profiled_when_its_command_line_names_a_directory() {
    let with = |env: Vec<(String, String)>| {
        let mut targets = targets();
        targets.cargo = Some(blaze_api::Cargo {
            command: "build".into(),
            args: Vec::new(),
            root: "/w".into(),
            cwd: "/w".into(),
            env,
            files: Vec::new(),
            color: false,
            width: None,
            verbose: 0,
            refused: Some(16),
        });
        Work::Build(targets)
    };
    let named = vec![("BLAZE_PROFILE".into(), "/tmp/p".into()), ("PATH".into(), "/bin".into())];
    assert_eq!(profile_dir(&with(named)), Some("/tmp/p"));
    assert_eq!(profile_dir(&with(vec![("BLAZE_PROFILE".into(), String::new())])), None);
    assert_eq!(profile_dir(&with(vec![("PATH".into(), "/bin".into())])), None);
    assert_eq!(profile_dir(&Work::Build(targets())), None);
    assert_eq!(profile_dir(&Work::Snapshot), None);
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
    let work = Work::Test { targets: targets(), filter: Vec::new(), all: true, args: Vec::new(), ub };
    assert_eq!(command_of(&work), Command::Test);
    assert_eq!(goal_of(&work), Goal::Check);
    let compiled = Work::Test { targets: targets(), filter: Vec::new(), all: true, args: Vec::new(), ub: None };
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
    assert_eq!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/r/serde-1.0.0", "aa")));
    assert_ne!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/r/serde-1.0.0", "bb")));
    assert_ne!(fetch_key(&fetch("/r/serde-1.0.0", "aa")), fetch_key(&fetch("/s/serde-1.0.0", "aa")));
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
    let mut slab: Tagged<u8> = Tagged::new(OWN_SLOT_BITS, OWN_GENERATION_BITS, MemTag::Other).unwrap();
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
    let (a, b, c) = (JobId { slot: 0, generation: 0 }, JobId { slot: 1, generation: 0 }, JobId { slot: 0, generation: 1 });
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
    let mut paths = Paths::<Real>::new();
    let a = paths.intern("/w/target/debug/a");
    let b = paths.intern("/w/target/debug/b");
    assert_eq!(paths.intern("/w/target/debug/a"), a);
    assert_ne!(a, b);
    assert_eq!(paths.files[a as usize].as_ref().map(ToString::to_string).as_deref(), Some("w/target/debug/a"));
    assert_eq!(paths.find(&[b, a], "/w/target/debug/a"), a);
    let c = paths.find(&[b], "/w/target/debug/c");
    assert_eq!(&*paths.names[c as usize], "/w/target/debug/c");
    assert!(paths.take_images(&[a, b, c]).is_empty());
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
