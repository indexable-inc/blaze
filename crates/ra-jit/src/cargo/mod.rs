//! `cargo blaze build|run|test|check`: cargo's commands, built whole by the daemon, which
//! builds what changed since it last built. `docs/cargo.md` is the design.

pub(crate) mod daemon;
pub(crate) mod wrapper;

mod affected;
mod ancestor;
mod cli;
mod client;
pub(crate) mod layout;
mod messages;
pub(crate) mod metadata;
mod own;
pub(crate) mod own_macro;
pub(crate) mod own_script;
mod package;
pub(crate) mod plan;
pub(crate) mod run;
pub(crate) mod sandboxed;
pub(crate) mod say;
pub(crate) mod script;
pub(crate) mod script_sandbox;
mod state;
pub(crate) mod switch;
mod toolchain;
mod units;
mod whole;

use std::{collections::BTreeMap, time::Instant};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, Reply, Request};
pub(crate) use units::Record;

/// `ra-jit cargo ...`: runs the command and ends with the status cargo would end with.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args: Vec<String> = args.collect();
    let asked = Asked::parse(own::subcommand_first(args));
    if !matches!(asked, Asked::Build(_)) {
        crate::end_with_reader();
    }
    if let Asked::Build(_) = asked
        && let Err(why) = toolchain::settle()
    {
        say::error(&why);
        std::process::exit(FAILED);
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
        Asked::Enable => switch::enable()?,
        Asked::Disable => switch::disable()?,
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
        Asked::Help => {
            println!("{HELP}");
            0
        }
        Asked::Version => {
            println!("{}", crate::version());
            0
        }
    };
    std::process::exit(code)
}

const HELP: &str = "\
blaze: cargo's build, run, test and check, from a daemon that keeps the workspace compiled

usage: blaze <command> [cargo's flags]    (or cargo blaze <command>)

  build, run         each program built whole by blaze, with no rustc and no cargo build
  test               each test built whole by blaze and run as cargo runs it, then the doc
                     tests, each example built by blaze; no rustc, rustdoc or cargo build
  check              what the daemon finds loading the workspace as the build would, with no
                     rustc
  status             whether blaze is enabled, what the daemon has loaded
  stop               ends the daemon; the next build starts one
  enable, disable    plain `cargo` is blaze, and back
  toolchain          blaze's own toolchain, which it builds every project with, downloaded once
                     if it has none (`--path`: its directory)
  cache              what the `#[ra_jit::cache]` functions kept, by function
  cache clear        forgets it; `--stale`, all but what each function's code computed

The daemon starts on the first build and stops after 30 minutes without one. Every other
command of cargo's is blaze's own, or fails saying blaze has none yet: none is handed to cargo
(`blaze --list`). The guide: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md";

/// `cargo ra-jit cache`: what the `#[ra_jit::cache]` functions kept, which is under the target
/// directory unless `RA_JIT_FN_CACHE_DIR` says where; `cache clear` forgets it, and `cache clear
/// --stale` all but what each function's code computed. Any other verb is one of `ra-jit cache`,
/// on the disk store.
fn cache(args: Vec<String>) -> anyhow::Result<i32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stale = match args[..] {
        [] => return list_kept(&fn_cache_dir()?).map(|()| 0),
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
    Ok(match std::env::var_os("RA_JIT_FN_CACHE_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let metadata = metadata::no_deps(None, &[]);
            metadata.target_directory.as_std_path().join("ra-jit").join("fn-cache")
        }
    })
}

/// `cargo blaze check`: what the daemon's checker says of the source, loading the workspace
/// as the build of the same flags would and building nothing, once for each target asked
/// for. No rustc runs on the source. The verdict is written
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
        let finished = format!("`{profile}` profile [ra-jit] target(s) in {took:.2}s");
        say::status(asked.flags.quiet, "Finished", &finished);
        messages::passed(format);
    };
    let turned_down = |said: &[crate::errors::Said]| -> anyhow::Result<i32> {
        messages::turned_down(format, said, &state::metadata(asked));
        toolchain::note_failed();
        Ok(FAILED)
    };
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
                eprintln!("error: ra-jit could not check the workspace: {reason}");
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

/// `checkout` as a request names it: with the variables of this process, which `env!`
/// reads where cargo set none.
fn asking(checkout: &Checkout) -> Checkout {
    // Not those that say what blaze prints of the build: one that changed would be an edit
    // of every `env!`.
    let wraps = |name: &str| name == "RA_JIT_STATS" || name == "RA_JIT_TRACE";
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| !wraps(name)).collect();
    env.extend(checkout.env.clone());
    Checkout { env, ..checkout.clone() }
}

/// What the store of compiled code holds and may hold, and where it is.
fn store_status() -> String {
    let Some(dir) = daemon::store() else {
        return "store: none (RA_JIT_CACHE says so)".to_owned();
    };
    let held = ra_jit_store::StoreDir::at(&dir).stats().map_or(0, |it| it.held());
    let most = match ra_jit_store::limit_for(&dir) {
        Ok(Some(most)) => format!("at most {} MB", most >> 20),
        Ok(None) => "no limit".to_owned(),
        Err(why) => why,
    };
    format!("store: {} MB, {most} (`ra-jit cache gc` collects it), {}", held >> 20, dir.display())
}

/// `cargo ra-jit status`: the daemon, its workers, the store, and how the builds of each
/// checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    for line in switch::status() {
        let _ = writeln!(said, "{line}");
    }
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
        }
        Some(Reply::Restart { version }) => {
            let _ = writeln!(
                said,
                "daemon: another build of ra-jit, version {version}; the next build replaces it"
            );
        }
        _ => said.push_str("daemon: not running\n"),
    }
    let _ = writeln!(said, "{}", store_status());
    // Whoever reads this may stop reading before its end.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), said.as_bytes());
    Ok(0)
}

/// `cargo ra-jit stop`: has the daemon stop its workers and exit.
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
