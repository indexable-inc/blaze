//! Every exit code a blaze process ends with on purpose, in one list ([`Exit`]): the CLI, the
//! daemon (`blaze serve`), the sandbox launchers and the children blaze forks before they exec.
//!
//! Each code is one line of `exits!`, with its doc (what the process did and why it ended),
//! its name and its one-line meaning; the enum, [`Exit::from_code`], [`Exit::name`] and
//! [`Exit::describe`] are made from that line, so none is kept in sync by hand. The enum is
//! `#[repr(i32)]`, so two codes that collide are refused by the compiler. Every code is below
//! 128, where a shell's statuses for a process a signal ended begin ([`INTERRUPTED`]).
//!
//! The numbers are observed by scripts and tests: a code, once given, keeps its number.

use std::fmt;

/// Makes [`Exit`] and its tables from one list: `Variant = code, "NAME", "meaning";`.
macro_rules! exits {
    ($($(#[$doc:meta])* $variant:ident = $code:literal, $name:literal, $meaning:literal;)*) => {
        /// An exit code a blaze process ends with on purpose (the module's doc).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(i32)]
        pub enum Exit {
            $($(#[$doc])* $variant = $code,)*
        }

        impl Exit {
            /// Every code, in the list's order.
            pub const ALL: &'static [Exit] = &[$(Exit::$variant),*];

            /// The code that names `code`; `None` for a code that is none of blaze's.
            #[must_use]
            pub const fn from_code(code: i32) -> Option<Exit> {
                match code {
                    $($code => Some(Exit::$variant),)*
                    _ => None,
                }
            }

            /// Its name, as the source said it before the list: `LOOP_STALLED`.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Exit::$variant => $name,)*
                }
            }

            /// What a process that ended with it did, in one line.
            #[must_use]
            pub const fn describe(self) -> &'static str {
                match self {
                    $(Exit::$variant => $meaning,)*
                }
            }
        }
    };
}

exits! {
    /// The CLI refused its command line and did nothing: clap's code for a command line it
    /// refuses.
    Usage = 1, "USAGE", "its command line was refused";
    /// The daemon's stop made no progress for its bound (`serve::stop`): it ends without its
    /// image, having written what it was inside.
    StopStalled = 75, "STOP_STALLED", "its stop made no progress for the stall bound, and it ended without its image";
    /// The daemon's loop did not finish a turn while nothing else moved for its stall bound
    /// (`serve::stop`): deadlocked, it reaped no child and answered no client, and ends having
    /// written what it was inside and which children it left unreaped.
    LoopStalled = 76, "LOOP_STALLED", "its loop made no progress for the stall bound";
    /// The daemon's footprint passed its memory ceiling (docs/memory-budget.md section 7, "The
    /// ceiling, enforced past admission"): its own watch (`serve::stop`) wrote what it held, by
    /// tag, and what each thread was inside, then ended it rather than let it take the machine
    /// down.
    MemoryCeiling = 77, "MEMORY_CEILING", "its footprint passed its memory ceiling";
    /// A build or test failed, or the CLI met an error of its own: cargo's code for both.
    Failed = 101, "FAILED", "a build or test failed, or blaze met an error of its own";
    /// A sandbox launcher could not make the sandbox, so the program it was given did not run:
    /// the code `env` ends with when it can't run what it is given.
    CouldNot = 125, "COULD_NOT", "its sandbox could not be made, so the program did not run";
    /// A child blaze forked could not exec its program (it is missing, not executable, or
    /// still open for writing): a shell's code for a command it could not run.
    ExecFailed = 127, "EXEC_FAILED", "it could not exec its program";
}

impl Exit {
    /// Its number, the status the process ends with.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// `76 (LOOP_STALLED: its loop made no progress for the stall bound)`.
impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}: {})", self.code(), self.name(), self.describe())
    }
}

/// The status of a process that caught an interrupt (Ctrl-C, SIGINT, 2 on every system blaze
/// runs on) and ended for it: a shell's `128 + SIGINT`, as if the signal had ended it. It is
/// no [`Exit`]: it says a signal, which every `Exit` stays below.
pub const INTERRUPTED: i32 = 128 + 2;

/// A process's exit code as said to a person: one of blaze's with its name and meaning
/// ([`Exit`]'s `Display`), any other as its number.
#[must_use]
pub fn said(code: i32) -> Said {
    Said(code)
}

/// What [`said`] returns.
#[derive(Clone, Copy, Debug)]
pub struct Said(i32);

impl fmt::Display for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match Exit::from_code(self.0) {
            Some(exit) => exit.fmt(f),
            None => write!(f, "{}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_exit_code_round_trips_and_stays_below_signal_deaths() {
        for &exit in Exit::ALL {
            assert_eq!(Exit::from_code(exit.code()), Some(exit), "{exit}");
            assert!(
                (1..128).contains(&exit.code()),
                "{exit} is not a code below a signal death's 128"
            );
        }
        assert_eq!(Exit::from_code(0), None);
        assert_eq!(
            said(76).to_string(),
            "76 (LOOP_STALLED: its loop made no progress for the stall bound)"
        );
        assert_eq!(said(3).to_string(), "3");
    }
}
