//! What a daemon takes from how it was started and keeps for its life: the environment
//! variables it reads from its own process (once at its start, or into a `LazyLock` on first
//! use), never from a request's [`blaze_api::Cargo::env`]. A daemon is started by whichever
//! client finds none, with that client's environment; a later client with other values would
//! build under the first one's (a client with `BLAZE_DAEMON_MEMORY_GB=100` talked to a daemon
//! started with 80, which kept 80 and failed in memory admission ten minutes into the build).
//!
//! So the daemon says what it was started with ([`blaze_api::Status::settings`], made by
//! [`of_daemon`]), and a client that may start a daemon compares the settings it sets with
//! that ([`of_client`], [`drift`]) before it sends any request (`cli::daemon::connect`): a
//! setting the client sets to another value is refused at once, naming each setting, both
//! values and the fix ([`said`], `cli::daemon::DaemonError::Drifted`). A setting the client
//! leaves unset is the daemon's to choose: a daemon started with `--memory-gb 80` serves every
//! client that does not set `BLAZE_DAEMON_MEMORY_GB` itself. The daemon is never restarted over
//! a difference: it serves every lane of a home, and a restart would cut off a build that
//! arrived meanwhile and let two lanes of other values take turns restarting it. Socket units
//! capture these settings from the shell that installs them (`blaze_api::units::here`).
//!
//! What is not here, and why: variables a request carries (`BLAZE_SANDBOX`, `BLAZE_DEPS`,
//! `BLAZE_ELIDE`, `BLAZE_ITEM_STORE`, `BLAZE_DEP_IMAGES`, `BLAZE_SCRIPT_CACHE`, `BLAZE_PURE`,
//! `BLAZE_VERIFY_STORE`, `BLAZE_PROFILE`'s directory), which the daemon reads per request;
//! `BLAZE_HOME`, which names the socket, so another home is another daemon; `BLAZE_API_TOKEN`,
//! a secret, read only with `--http`, which no client starts; `BLAZE_STATS`, which each command
//! also reads for itself (`cli.rs`), so one command asking for its counters must not be
//! refused; `PATH` and `HOME`, which the daemon also keeps (`serve::engine`) but which differ
//! between shells all the time, so a refusal on each would refuse most commands.

use blaze_api::Setting;

/// Every variable the daemon reads from its own environment and keeps, in the order
/// [`blaze_api::Status::settings`] has them. Each names where it is read.
pub const STARTED_WITH: &[&str] = &[
    // The memory limit (`serve::engine`, `told_limit`): `--memory-gb` wins where given.
    super::MEMORY_VAR,
    // The cores the plane and the worker pool are sized to, and the process's priority
    // (blaze-core's `cores::get`, read once and lowered at the start): compared by the budget
    // and priority each gives ([`same`]).
    blaze_core::cores::VAR,
    // The DuckDB the explorer and live SQL run (`serve::engine`, read once).
    super::observe::DUCKDB,
    // The kept expansions' check (proc-macro-api's `kept.rs`, at the daemon's setup).
    proc_macro_api::kept::CACHE,
    // Huge pages (blaze-mem's `huge.rs`, chosen once).
    blaze_mem::huge::VAR,
    // The standard library's source (`toolchain::library`, `OnceLock`s).
    crate::toolchain::library::VAR,
    "BLAZE_LIBRARY_FROM",
    // hir-ty-codegen's knobs (`v2/backend.rs`'s `CHANGES_CODE` and each one's `LazyLock`).
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_EMIT",
    hir_ty_codegen::MIR_OPT,
    "BLAZE_REGALLOC",
    "BLAZE_INLINE",
    "BLAZE_INLINE_ASM",
    "BLAZE_LLVM_INLINE_THRESHOLD",
    "BLAZE_BUILT_IN",
    "BLAZE_ASSEMBLER",
    "BLAZE_PROMOTE",
    "BLAZE_LEAN",
    "BLAZE_NO_HOME",
    // The borrow checker's mode and its debugging (base-db's `settings.rs`, once per
    // database).
    "BLAZE_BORROWCK",
    "BLAZE_BORROWCK_DEBUG",
    "BLAZE_TRACE_HOLDERS",
    // Codegen's checks and dumps (hir-ty-codegen, each a `LazyLock` or read per call).
    "BLAZE_LAYOUTS",
    "BLAZE_CHECK_PLACES",
    "BLAZE_DUMP_MIR",
    "BLAZE_DUMP_CLIF",
    "BLAZE_DUMP_TIER",
    "BLAZE_DUMP_LLVM",
    "BLAZE_FN_CACHE_TRACE",
    // The Lean that checks proofs (hir-ty-lean).
    "BLAZE_LEAN_PROVER",
    // blaze-alloc-count's tracing (`OnceLock`).
    "BLAZE_ALLOC_TRACE",
];

/// The value of `name` in an environment, as text: `None` unset, a value that is not UTF-8
/// with its bytes replaced (it is only compared and said).
fn value_of(name: &str, var: &impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<String> {
    var(name).map(|it| it.to_string_lossy().into_owned())
}

/// What the daemon runs with: each of [`STARTED_WITH`] from its environment (`var`), but the
/// memory limit as it was told it ([`super::told_limit`]'s `told`, `--memory-gb` first), its
/// number of GiB.
pub fn of_daemon(
    told: Option<&(u64, String)>,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<Setting> {
    STARTED_WITH
        .iter()
        .map(|&name| {
            let value = match name == super::MEMORY_VAR {
                true => told.map(|(_, said)| {
                    said.split_once('=').map_or(said.as_str(), |(_, gb)| gb).to_owned()
                }),
                false => value_of(name, &var),
            };
            Setting { name: name.to_owned(), value } // alloc: once per daemon start
        })
        .collect()
}

/// What a client asks for: each of [`STARTED_WITH`] from its environment (`var`), as the
/// daemon it starts would have it; `None` where it is unset, which asks for nothing.
pub fn of_client(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<Setting> {
    STARTED_WITH
        .iter()
        // alloc: once per command that may start the daemon
        .map(|&name| Setting { name: name.to_owned(), value: value_of(name, &var) })
        .collect()
}

/// One setting a client and the daemon it reached differ in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Drift {
    pub name: String,
    /// The daemon's value; `None` unset.
    pub theirs: Option<String>,
    /// The client's: only a setting it sets can differ.
    pub ours: String,
}

/// Whether two values of `name` say the same: the memory limit by the bytes each is (`80` and
/// `80.0`); the cores by the budget and priority each gives on this machine
/// ([`blaze_core::cores::Cores::agrees`]: unset and the default budget agree, `all` and every
/// core by number do not); every other by its text.
fn same(name: &str, a: Option<&str>, b: Option<&str>) -> bool {
    if name == blaze_core::cores::VAR
        && let Ok(machine) = blaze_core::cores::machine()
        && let (Ok(a), Ok(b)) = (
            blaze_core::cores::of(a.map(std::ffi::OsStr::new), machine),
            blaze_core::cores::of(b.map(std::ffi::OsStr::new), machine),
        )
    {
        return a.agrees(&b);
    }
    if name == super::MEMORY_VAR
        && let (Some(a), Some(b)) = (a, b)
        && let (Ok((a, _)), Ok((b, _))) = (super::told_memory(name, a), super::told_memory(name, b))
    {
        return a == b;
    }
    a == b
}

/// Each setting `ours` sets that the daemon's `theirs` has another value of, or none, in
/// [`STARTED_WITH`]'s order. A setting `ours` leaves unset is never drift: the command takes
/// the daemon's. A setting the daemon does not say is unset there.
pub fn drift(ours: &[Setting], theirs: &[Setting]) -> Vec<Drift> {
    ours.iter()
        .filter_map(|our| {
            let asked = our.value.as_deref()?;
            let their =
                theirs.iter().find(|it| it.name == our.name).and_then(|it| it.value.as_deref());
            (!same(&our.name, Some(asked), their)).then(|| Drift {
                name: our.name.clone(), // clone: an error's, once per setting that differs
                theirs: their.map(str::to_owned),
                ours: asked.to_owned(), // alloc: as above
            })
        })
        .collect()
}

/// `BLAZE_DAEMON_MEMORY_GB: the daemon has 80, this command 100; BLAZE_OPT: the daemon has it
/// unset, this command 2`.
pub fn said(drift: &[Drift]) -> String {
    let value = |it: &Option<String>| match it {
        Some(it) => format!("`{it}`"),
        None => "it unset".to_owned(),
    };
    drift
        .iter()
        .map(|it| {
            // alloc: an error's or a line's text, once
            format!("{}: the daemon has {}, this command `{}`", it.name, value(&it.theirs), it.ours)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(it, _)| *it == name)
                .map(|(_, value)| std::ffi::OsString::from(*value))
        }
    }

    /// The client of 2026-11-03 asked for 100 GiB of a daemon told 80 by `--memory-gb`: the
    /// one setting that differs is named with both values. The same limit written another way
    /// (`80.0`), settings both leave unset, and settings the client leaves unset (the daemon's
    /// `--memory-gb` and `BLAZE_OPT` stand) are no drift; one the client sets that the daemon
    /// does not is.
    #[test]
    fn a_daemon_told_another_limit_drifts_by_that_setting_alone() {
        let told = super::super::told_memory("--memory-gb", "80").unwrap();
        let daemon =
            of_daemon(Some(&told), env(&[(super::super::MEMORY_VAR, "60"), ("BLAZE_OPT", "2")]));
        assert_eq!(daemon.len(), STARTED_WITH.len());
        let memory = daemon.iter().find(|it| it.name == super::super::MEMORY_VAR).unwrap();
        assert_eq!(
            memory.value.as_deref(),
            Some("80"),
            "the flag wins over the variable, as the budget does"
        );

        let ours = of_client(env(&[(super::super::MEMORY_VAR, "100"), ("BLAZE_OPT", "2")]));
        let drifted = drift(&ours, &daemon);
        assert_eq!(
            drifted,
            [Drift {
                name: super::super::MEMORY_VAR.to_owned(),
                theirs: Some("80".to_owned()),
                ours: "100".to_owned()
            }]
        );
        assert_eq!(
            said(&drifted),
            "BLAZE_DAEMON_MEMORY_GB: the daemon has `80`, this command `100`"
        );

        let same_limit = of_client(env(&[(super::super::MEMORY_VAR, "80.0"), ("BLAZE_OPT", "2")]));
        assert!(drift(&same_limit, &daemon).is_empty());

        let unset = of_client(env(&[]));
        assert!(
            drift(&unset, &daemon).is_empty(),
            "a command that sets nothing takes the daemon's"
        );

        let more = of_client(env(&[("BLAZE_LEAN", "1")]));
        assert_eq!(
            said(&drift(&more, &daemon)),
            "BLAZE_LEAN: the daemon has it unset, this command `1`"
        );
    }

    /// `BLAZE_CORES` is one of the settings (PR #1669 refused it on its own): a command that
    /// sets the budget the daemon's unset value gives agrees; `all` is drift (normal priority,
    /// not background), named in the one list beside any other.
    #[test]
    fn cores_drift_by_budget_and_priority_beside_the_other_settings() {
        let machine = blaze_core::cores::machine().unwrap();
        let default = blaze_core::cores::default_budget(machine).to_string();
        let daemon = of_daemon(None, env(&[]));
        let ours = of_client(move |name| {
            (name == blaze_core::cores::VAR).then(|| std::ffi::OsString::from(&default))
        });
        assert!(
            drift(&ours, &daemon).is_empty(),
            "the default budget by number is the daemon's unset"
        );
        let all = of_client(env(&[(blaze_core::cores::VAR, "all"), ("BLAZE_LEAN", "1")]));
        assert_eq!(
            said(&drift(&all, &daemon)),
            "BLAZE_CORES: the daemon has it unset, this command `all`; BLAZE_LEAN: the daemon has it unset, this command `1`"
        );
    }
}
