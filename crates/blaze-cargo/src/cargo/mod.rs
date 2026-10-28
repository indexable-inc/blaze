//! `blaze build|run|test|check`: cargo's commands, with the daemon building what
//! changed since cargo last built. `docs/cargo.md` is the design.

pub mod daemon;
pub mod keyed;
pub mod load_wrapper;

pub mod affected;
pub mod backend;
pub mod cli;
pub mod client;
#[cfg(unix)]
pub mod closure;
pub mod compile_time;
pub mod directives;
pub mod experimental;
pub mod fetches;
#[cfg(unix)]
pub mod hermetic;
pub mod home;
pub mod layout;
pub mod messages;
pub mod metadata;
pub mod own;
pub mod own_macro;
pub mod own_script;
pub mod package;
pub mod plan;
pub mod report;
pub mod results;
pub mod run;
pub mod sandboxed;
pub mod say;
pub mod script;
pub mod script_sandbox;
pub mod state;
pub mod toolchain;
pub mod units;
pub mod wasi_script;
pub mod whole;
pub mod why;

use std::{collections::BTreeMap, time::Instant};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, NotBuilt, Reply, Request};
use state::{Held, State};
pub use toolchain::{ours_commit, ours_says};
pub use units::Record;

/// `blaze-engine cargo ...`: runs the command and ends with the status cargo would end with.
pub fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args: Vec<String> = args.collect();
    let asked = Asked::parse(own::subcommand_first(args));
    if !matches!(asked, Asked::Build(_) | Asked::Cache(_)) {
        crate::end_with_reader();
    }
    if let Ok(home) = daemon::home() {
        crate::renamed::say_old_dirs(&home);
    }
    if let Err(why) = home::undo_enable() {
        say::error(&format!("{why:#}"));
        std::process::exit(FAILED);
    }
    if let Asked::Build(asked) = &asked {
        say::quiet(asked.flags.quiet);
        if let Some(format) = &asked.flags.events
            && let Err(why) = say::events(format)
        {
            say::error(&why);
            std::process::exit(FAILED);
        }
        let cwd = std::env::current_dir()?;
        layout::first_unless_chosen(&cwd, &asked.flags.config, &asked.flags.unstable);
    }
    if let Asked::Build(asked) = &asked
        && let Err(why) = toolchain::settle().and_then(|_| backend::follow_profile(asked))
    {
        say::error(&why);
        std::process::exit(FAILED);
    }
    if let Asked::Build(build) = &asked
        && let Ok(cwd) = std::env::current_dir()
    {
        toolchain::say_toolchain_file(&cwd, build.flags.verbose);
    }
    let code = match asked {
        Asked::Build(asked) if matches!(asked.subcommand, Subcommand::Build | Subcommand::Run) => {
            whole::build(&asked)?
        }
        Asked::Build(asked) if asked.subcommand == Subcommand::Test => whole::test(&asked)?,
        Asked::Build(asked) => check(&asked)?,
        Asked::Own(args) => own::command(args)?,
        Asked::Cache(args) => cache(args)?,
        Asked::Status => status()?,
        Asked::Stop => stop()?,
        Asked::Why(args) => why::command(&args)?,
        Asked::Watch => watch()?,
        Asked::Toolchain { path } => match toolchain::settle() {
            Ok(chosen) if path => {
                println!("{}", chosen.dir().display());
                0
            }
            Ok(chosen) => {
                println!("blaze builds with {chosen}");
                0
            }
            Err(why) => {
                say::error(&why);
                FAILED
            }
        },
        Asked::Lock(args) => fetches::command(&args)?,
        Asked::Pin(args) => (crate::engine::get().pin)(&args)?,
        Asked::Help => {
            println!("{HELP}");
            0
        }
        Asked::Version => {
            println!("{}", crate::version());
            println!("{}", (crate::engine::get().why_this_one)());
            0
        }
    };
    std::process::exit(code)
}

const HELP: &str = "\
blaze: cargo's build, run, test and check, from a daemon that keeps the workspace compiled

usage: blaze <command> [cargo's flags]    (or blaze <command>)
       --events json|cbor   the records of the build on standard output as they come

  build, run         each program built whole by blaze, with no rustc and no cargo build
  test               each test built whole by blaze and run as cargo runs it, then the doc
                     tests, each example built by blaze; no rustc, rustdoc or cargo build
  check              what the daemon's build of the same flags says, when it has one loaded;
                     otherwise what the daemon finds loading the workspace as it, with no rustc
  status             what the daemon has loaded, the store, how the builds went
  stop               ends the daemon; the next build starts one
  why [fn|crate]     what the last build compiled again and why
  watch              every build the daemon does, as it does it
  toolchain          blaze's own toolchain, which it builds every project with, downloaded once
                     if it has none (`--path`: its directory)
  lock --fetches     records what build scripts download, by sha256, in Cargo.fetches.lock
                     (`--target <triple>`, the host's by default); a build hands it to them
  pin                this blaze's version in the workspace's Cargo.toml, under
                     `[workspace.metadata.blaze]`: `blaze` there runs this release, fetched
                     once where it is missing
  cache              what the `#[blaze::cache]` functions kept, by function
  cache clear        forgets it; `--stale`, all but what each function's code computed

The daemon starts on the first build and stops after 30 minutes without one. Every other
command of cargo's is blaze's own, or fails saying blaze has none yet: none is handed to cargo
(`blaze --list`). The guide: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md";

/// `blaze cache`: what the `#[blaze::cache]` functions kept, which is under the target
/// directory unless `BLAZE_FN_CACHE_DIR` says where; `cache clear` forgets it, and `cache clear
/// --stale` all but what each function's code computed. Any other verb is one of `blaze-engine cache`,
/// on the disk store.
fn cache(args: Vec<String>) -> anyhow::Result<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stale = match args[..] {
        [] => {
            crate::end_with_reader();
            return list_kept(&fn_cache_dir()?).map(|()| 0);
        }
        ["clear"] => false,
        ["clear", "--stale"] => true,
        _ => {
            crate::remote::command(args.into_iter().map(str::to_owned))?;
            return Ok(0);
        }
    };
    let dir = fn_cache_dir()?;
    if stale {
        let (results, bytes) = fn_cache::__private::clear_stale(&dir);
        println!("forgot {results} results of older code ({})", size(bytes));
        return Ok(0);
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => println!("cleared the kept results in {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("no results are kept"),
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dir.display())),
    }
    Ok(0)
}

/// What each cached function holds: the results of its code, and of the older codes kept for an
/// edit taken back.
fn list_kept(dir: &std::path::Path) -> anyhow::Result<()> {
    let (held, other) = fn_cache::__private::holdings(dir);
    if held.is_empty() && other.0 == 0 {
        println!("no results are kept");
        return Ok(());
    }
    println!("kept in {}", dir.display());
    for function in &held {
        let Some(&(now, now_bytes)) = function.codes.first() else { continue };
        let older = &function.codes[1..];
        let (old, old_bytes) = older.iter().fold((0, 0), |(n, b), it| (n + it.0, b + it.1));
        print!("  {}: {now} results ({})", function.name, size(now_bytes));
        if old > 0 {
            print!(", and {old} of {} older code ({})", older.len(), size(old_bytes));
        }
        println!();
    }
    if other.0 > 0 {
        println!("  {} results of code no function has now ({})", other.0, size(other.1));
    }
    Ok(())
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1048576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1048576.0),
    }
}

fn fn_cache_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(match std::env::var_os("BLAZE_FN_CACHE_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let metadata = metadata::no_deps(None, &[]);
            metadata.target_directory.as_std_path().join("blaze").join("fn-cache")
        }
    })
}

/// The status of a build that failed, as cargo's of one rustc turned down.
const FAILED: i32 = 101;

/// What the `Finished` line says the build was: blaze's, and the remote's failure if the
/// daemon's last exchange with it failed (`BLAZE_REMOTE=off` stops using it).
fn blaze_tag() -> String {
    match crate::remote::noted_down() {
        Some(down) => format!("blaze, remote down: {down}"),
        None => "blaze".to_owned(),
    }
}

/// The status a build that is over ends with when the daemon's remote failed and
/// `BLAZE_REMOTE_REQUIRED` makes that a failure of the build.
fn remote_failed_it() -> Option<i32> {
    let down = crate::remote::noted_down().filter(|_| crate::remote::required())?;
    say::error(&format!("the remote failed and BLAZE_REMOTE_REQUIRED is set: {down}"));
    Some(FAILED)
}

/// `blaze check`: what the daemon says of the source as it builds the programs of
/// the build with the same flags, when it has that build loaded. Any other check is the
/// daemon's checker's, which loads the workspace as that build would and builds nothing,
/// once for each target asked for. Neither runs rustc on the source. The verdict is written
/// as `--message-format` asks: in cargo's JSON, it is what an editor shows.
fn check(asked: &Invocation) -> anyhow::Result<i32> {
    if asked.flags.cargos_plan {
        say::error("`--build-plan` and `--unit-graph` are cargo's plans, and blaze makes its own");
        return Ok(FAILED);
    }
    let format = match messages::Format::of(&asked.flags.message_format) {
        Ok(format) => format,
        Err(why) => {
            eprintln!("error: {why}");
            return Ok(1);
        }
    };
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let finished = |profile: &str| {
        let took = started.elapsed().as_secs_f64();
        let finished = format!("`{profile}` profile [{}] target(s) in {took:.2}s", blaze_tag());
        say::status(asked.flags.quiet, "Finished", &finished);
        messages::passed(format);
    };
    let turned_down = |said: &[crate::errors::Said]| -> anyhow::Result<i32> {
        messages::turned_down(format, said, &state::metadata(asked));
        Ok(FAILED)
    };
    if asked.subcommand == Subcommand::Check
        && let Some(state) = State::read(&asked.key(&cwd))
        && !state.programs.is_empty()
    {
        match by_daemon(state) {
            Ok((state, _held)) => {
                finished(&state.profile);
                state.write()?;
                return Ok(0);
            }
            Err(NotServed::Error(said)) => return turned_down(&[said]),
            Err(NotServed::Stale) => {}
        }
    }
    let ahead = state::checked(asked)?;
    let profile = ahead.config.selection.profile.clone();
    let targets = match &asked.flags.targets[..] {
        [_, _, ..] => asked.flags.targets.iter().cloned().map(Some).collect(),
        _ => vec![ahead.config.selection.target.clone()],
    };
    let mut said = Vec::new();
    for target in targets {
        let mut ahead = ahead.clone();
        ahead.config.selection.target = target;
        match client::ask(Request::Check(ahead), Start::IfMissing)? {
            Some(daemon::Reply::Checked { errors: None }) => {}
            Some(daemon::Reply::Checked { errors: Some(errors) }) => said.push(errors),
            Some(daemon::Reply::Cold { reason }) => {
                eprintln!("error: blaze could not check the workspace: {reason}");
                return Ok(FAILED);
            }
            reply => anyhow::bail!("the daemon answered a check with {reply:?}"),
        }
    }
    if said.is_empty() {
        finished(&profile);
        return Ok(0);
    }
    turned_down(&said)
}

/// Why the daemon's programs are not the build.
enum NotServed {
    /// The daemon did not bring them up to date: something cargo acts on changed, or it did
    /// not build one of them. A check loads the workspace instead.
    Stale,
    /// The source has errors.
    Error(crate::errors::Said),
}

/// Has the daemon bring the programs of `state` up to date and puts them where cargo has
/// them. Rustc sees none of it: the daemon builds what rust-analyzer passes, and its verdict
/// on the source is the build's.
fn by_daemon(mut state: State) -> Result<(State, Held), NotServed> {
    use crate::trace::{CLIENT, note};
    let checking = Instant::now();
    let cargos = state.refusal.is_some() || state.broken_guard().is_some();
    if cargos || state.stale_cargos().is_some() {
        return Err(NotServed::Stale);
    }
    note("client checks", CLIENT, checking);
    // What the daemon did not build for an error in the source is not built: an item
    // rust-analyzer does not resolve is blaze's to fix, not rustc's to build.
    let answers = match state.checkouts.iter().map(built).collect::<Result<Vec<_>, _>>() {
        Ok(answers) => answers,
        Err(NotBuilt::Error(said)) => return Err(NotServed::Error(said)),
        Err(_) => return Err(NotServed::Stale),
    };
    let refused = state.programs.iter().any(|program| {
        program.asked_as.is_some_and(|(checkout, at)| {
            answers.get(checkout).and_then(|it| it.get(at)).is_some_and(Result::is_err)
        })
    });
    if refused {
        return Err(NotServed::Stale);
    }
    let settling = Instant::now();
    let held = state.hold();
    let cargos = match state.settle(&answers, &held) {
        Ok(cargos) => cargos.len(),
        Err(_) => return Err(NotServed::Stale),
    };
    note("client settle", CLIENT, settling);
    let served = cargos < state.programs.len() || state.programs.is_empty();
    state.counts.served += u64::from(served);
    Ok((state, held))
}

/// Has the daemon build the programs of `checkout`, loading it first when it has not: of
/// each, `Ok` or why the daemon did not build it. `Err` is why none is built: the daemon
/// failed, or the source has an error.
pub fn built(checkout: &Checkout) -> Result<Vec<Result<(), String>>, NotBuilt> {
    if checkout.programs.is_empty() {
        return Ok(Vec::new());
    }
    let asking_at = Instant::now();
    let reply = client::ask(Request::Build(asking(checkout)), Start::IfMissing)
        .map_err(|it| NotBuilt::Failed(format!("{it:#}")))?;
    crate::trace::note("client↔daemon", crate::trace::CLIENT, asking_at);
    let programs = match reply {
        Some(Reply::Synced { programs, spans }) => {
            crate::trace::note_all(spans);
            for line in programs.iter().flatten() {
                crate::trace::note_all(crate::trace::split(line).1);
            }
            programs
        }
        None => return Err(NotBuilt::Failed(client::why_none())),
        Some(Reply::Cold { reason }) => return Err(NotBuilt::Failed(reason)),
        Some(other) => return Err(NotBuilt::Failed(format!("the daemon answered {other:?}"))),
    };
    std::iter::zip(&checkout.programs, programs)
        .map(|(program, built)| match built {
            Ok(_) => Ok(Ok(())),
            Err(NotBuilt::Error(said)) => Err(NotBuilt::Error(said)),
            Err(NotBuilt::Failed(why)) => {
                Ok(Err(format!("blaze did not build {}: {why}", program.binary.display())))
            }
            Err(NotBuilt::Cargos(why)) => Ok(Err(why)),
        })
        .collect()
}

/// `checkout` as a request names it: with the variables of this process, which `env!`
/// reads where cargo set none, but those that say what blaze prints of the build: one that
/// changed would be an edit of every `env!`.
fn asking(checkout: &Checkout) -> Checkout {
    let printing = |name: &str| name == "BLAZE_STATS" || name == "BLAZE_TRACE";
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| !printing(name)).collect();
    env.extend(checkout.env.clone());
    Checkout { env, ..checkout.clone() }
}

/// What the store of compiled code holds and may hold, and where it is.
fn store_status() -> String {
    let Some(dir) = daemon::store() else {
        return format!("store: none ({})", daemon::no_store());
    };
    let held = blaze_store::StoreDir::at(&dir).stats().map_or(0, |it| it.held());
    let most = match blaze_store::limit_for(&dir) {
        Ok(Some(most)) => format!("at most {} MB", most >> 20),
        Ok(None) => "no limit".to_owned(),
        Err(why) => why,
    };
    format!("store: {} MB, {most} (`blaze cache gc` collects it), {}", held >> 20, dir.display())
}

/// The build under way in the workspace of the current directory, or its last, as cargo's
/// lines: what `blaze status` says of it.
fn build_here() -> anyhow::Result<String> {
    let here = std::env::current_dir()?;
    let root = dunce::canonicalize(&here).unwrap_or(here);
    let subscribe = blaze_protocol::events::Subscribe::Build { root };
    let request = Request::Watch { subscribe, follow: false };
    let Ok(Some(Reply::Watched(records))) = client::ask(request, Start::Never) else {
        return Ok(String::new());
    };
    let Some(first) = records.first() else { return Ok(String::new()) };
    let blaze_protocol::events::Event::Began { command, root, .. } = &first.event else {
        return Ok(String::new());
    };
    let finished =
        records.iter().any(|it| matches!(it.event, blaze_protocol::events::Event::Finished { .. }));
    let when = if finished { "last" } else { "under way" };
    let mut said = format!("  {command} of {} ({when}):\n", root.display());
    // What the live view of it shows, as of its last record.
    let mut live = blaze_ui::Live::new(100, false);
    for record in &records {
        live.record(record, record.at_ns);
    }
    for line in live.snapshot(live.build().last_ns) {
        said.push_str(&format!("    {line}\n"));
    }
    Ok(said)
}

/// `blaze watch`: every build the daemon does, shown as it does it, until interrupted.
fn watch() -> anyhow::Result<i32> {
    say::watching();
    let subscribe = blaze_protocol::events::Subscribe::All;
    match client::ask(Request::Watch { subscribe, follow: true }, Start::Never)? {
        Some(Reply::Watched(_)) => Ok(0),
        Some(Reply::Restart { version }) => {
            eprintln!("error: the daemon is another build of blaze, version {version}");
            Ok(FAILED)
        }
        _ => {
            eprintln!("error: {}", client::why_none());
            Ok(FAILED)
        }
    }
}

/// `blaze status`: the daemon, its workers, the store, and how the builds of each
/// checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    match client::ask(Request::Status, Start::Never)? {
        Some(Reply::Status(daemon)) => {
            let idle =
                daemon.idle_secs.map_or("never".to_owned(), |it| format!("after {it}s idle"));
            let most = daemon.memory_max.map_or("no limit".to_owned(), |it| {
                format!("workers hold at most {} MB", it >> 20)
            });
            let _ = writeln!(
                said,
                "daemon: pid {}, version {}, exits {idle}, {most}",
                daemon.pid, daemon.version
            );
            for worker in daemon.workers {
                let holds =
                    worker.bytes.map_or(String::new(), |it| format!("holds {} MB, ", it >> 20));
                let _ = writeln!(
                    said,
                    "  worker: pid {}, {}, {holds}log {}",
                    worker.pid,
                    worker.project.display(),
                    worker.log.display()
                );
                for (root, warmth) in worker.checkouts {
                    let _ = writeln!(said, "    {}: {warmth}", root.display());
                }
            }
            said.push_str(&build_here()?);
        }
        Some(Reply::Restart { version }) => {
            let _ = writeln!(
                said,
                "daemon: another build of blaze, version {version}; the next build replaces it"
            );
        }
        _ => said.push_str("daemon: not running\n"),
    }
    let _ = writeln!(said, "{}", store_status());
    for state in State::all() {
        let to_cargo: u64 = state.counts.cargo.values().sum();
        let _ = writeln!(
            said,
            "{} ({}, {}): {} builds by the daemon, {to_cargo} by cargo",
            state.root.display(),
            state.profile,
            state.key,
            state.counts.served
        );
        for (reason, count) in &state.counts.cargo {
            let _ = writeln!(said, "  {count} x {reason}");
        }
        if let Some(refusal) = &state.refusal {
            let _ = writeln!(said, "  every build is cargo's: {refusal}");
        }
    }
    // Whoever reads this may stop reading before its end.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), said.as_bytes());
    Ok(0)
}

/// `blaze stop`: has the daemon stop its workers and exit.
fn stop() -> anyhow::Result<i32> {
    match client::ask(Request::Stop, Start::Never)? {
        Some(_) => {
            client::gone(&daemon::socket_in(&daemon::home()?));
            println!("daemon: stopped");
        }
        None => println!("daemon: not running"),
    }
    Ok(0)
}
