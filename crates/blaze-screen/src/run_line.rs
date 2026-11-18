//! What a command does while it works, as events ([`Step`]) to one sink ([`Watch`]), and the one
//! live status line drawn from them (docs/terminal.md, "While `run` works"). The command
//! emits events and never draws: the sink is handed in from the command's edge (`blaze run`'s
//! main.rs), so the run's parent, its loop and the front end say what happens and a driver
//! decides what a terminal shows. Off a terminal the sink is [`Unwatched`], which keeps
//! nothing.
//!
//! The events are shaped for any command that works before it answers (`run` today; `check`
//! and `engine-check` later): a stage begun, the item being worked on, how a first call was
//! answered, and the three moments a program's own output may start (it was started, a need
//! is about to be answered, the command is about to write a line itself).

/// A stretch of a command's work before its items: what the status line names while no item
/// is being worked on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// The workspace checked (the gate: a body that is not clean is never built).
    Checking,
    /// The crates of the program found (`run_names::found`).
    Crates,
    /// The body ids read (the check's proofs, by item).
    BodyIds,
    /// The names records opened (the front end's tables of the program's crates).
    Names,
    /// The program's root instances found (`main`, the runtime's start, the specials).
    Roots,
}

impl Stage {
    /// The stage as the status line says it.
    pub const fn word(self) -> &'static str {
        match self {
            Stage::Checking => "checking",
            Stage::Crates => "finding crates",
            Stage::BodyIds => "reading body ids",
            Stage::Names => "opening names",
            Stage::Roots => "finding roots",
        }
    }
}

/// How a function's first call was answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answered {
    /// Its code record was in the store under its key.
    Found,
    /// It was built now and put under its key.
    Made,
    /// It was built now and has no key (a body read no record keys).
    Unkeyed,
}

/// One thing a command did, as it happens. `path` borrows the caller's words for the event's
/// moment only: a sink that keeps it copies it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<'a> {
    /// A stage begun.
    Began(Stage),
    /// `rows` functions placed ahead from today's start list: answered before they are asked.
    Placed { rows: u32 },
    /// A function's first call being answered: its item path when the sink wants paths
    /// ([`Watch::paths`]) and the front end names it, else `None`.
    Resolving { path: Option<&'a str> },
    /// That first call answered.
    Answered(Answered),
    /// The program is about to start: from now on it may write to the terminal.
    Started,
    /// The program asked (it is blocked on a need) and the command is answering it.
    Waits,
    /// The answer is about to be sent: the program may write again once it reads it.
    Answers,
    /// The command is about to write a line of its own: nothing live until the next event.
    Says,
}

/// The one sink a command's events go to. It is told each event as it happens, on the thread
/// that does the work; what it does with one must be quick, and a sink that draws must have
/// erased what it drew by the time it returns from [`Step::Started`], [`Step::Answers`] and
/// [`Step::Says`] (the terminal is the program's, or the command's own line's, after them).
pub trait Watch {
    /// One event.
    fn see(&self, step: Step<'_>);
    /// Whether [`Step::Resolving`] should carry the item's path (naming it costs a lookup in
    /// the names records: a sink that shows nothing asks for none).
    fn paths(&self) -> bool;
}

/// The sink that keeps nothing: a command off a terminal, and every caller with no sink.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Unwatched;

impl Watch for Unwatched {
    fn see(&self, _step: Step<'_>) {}

    fn paths(&self) -> bool {
        false
    }
}
