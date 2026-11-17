//! Each parameter is made ONCE, by its check (AGENTS.md rule 74: a check happens once and
//! returns a proof that later code takes).
//!
//! Before this, the launcher checked every parameter ([`Given`]'s `check`, which made the
//! capability and dropped it) and then made it again (`Filled::make`): a `Dir` grant opened its
//! folder twice, a `Net` grant parsed its endpoint twice, and a world that changed between the
//! two (a fault the simulation injected at the second open, a folder replaced by a file on a
//! machine) made `make` fail after its check had passed: an `unreachable!`, then an unwind
//! (`Unmade`), both the symptom of making twice.
//!
//! Now checking a parameter MAKES it and keeps what was made:
//!
//! - [`Checking`] is the launcher's state over the parameters checked so far. Its type holds
//!   the values made, in written order, as a left-nested tuple: after `a: A` and `b: B` it is a
//!   `Checking<(((), A), B)>`. [`Checking::check`] makes the next parameter from the spec and
//!   the worlds (the grant looked up once, the folder opened once) and adds it.
//! - [`Checking::filled`] adds the one problem no parameter can see (a grant for a parameter
//!   `main` lacks) and gives either every problem ([`Refused`]) or [`Filled`]: the proof that
//!   every parameter was made, which HANDS OUT the values the checks made. Nothing is made a
//!   second time, so nothing can fail between a check and a use.
//! - Every problem is still found before `main` starts: a parameter checked after one that
//!   failed is still made (that is its check) and is dropped at once.
//!
//! How a made value is unmade when another parameter fails: it is DROPPED, the values made so
//! far in the order they were made (a tuple drops its fields first to last), each later one as
//! soon as it is made. No kind holds anything a drop does not give back: a `Dir` is a handle on
//! the world, the folder's name (a refcount) and its quota (an `Arc`), and holds no descriptor
//! (a file opened through it does, and none is open yet); a `Net` is the world's handle and its
//! one endpoint; an `Env` the variable names it kept; `Out`, `In`, `Args`, `Clock`, `Mem` and
//! every whole capability a handle on the world (a refcount); a `Spec` the spec's `Arc`. So a
//! refused start leaks nothing and closes nothing twice.
//!
//! The macros' side ([`launch_body!`](crate::launch_body)): each parameter is checked in
//! written order, `filled()?` refuses or proves, and [`__made!`](crate::__made) is the pattern
//! that takes the values out by the parameters' own names:
//!
//! ```ignore
//! let checking = given.checking();
//! let checking = checking.check::<Out>("out");
//! let checking = checking.check::<Dir<ReadOnly>>("data");
//! let filled = checking.filled()?;
//! let cx = filled.cx();
//! let $crate::__made!(out, data) = filled.hand_out(); // `(((), out), data)`
//! main(cx, out, data)
//! ```

use blaze_own::{String, Vec};

use super::{Given, Grant, Granting, LaunchAlloc, Problem, Refused};
use crate::cx::Cx;

/// The parameters checked so far, each made once: `M` is what was made, in written order (the
/// module's doc), or, once any failed, every problem found so far.
pub struct Checking<'c, 'g, M> {
    given: &'c Given<'g>,
    made: Result<M, Vec<Problem, LaunchAlloc>>,
}

/// Proof that every parameter was made and that the spec grants nothing `main` does not take:
/// it hands out what the checks made.
pub struct Filled<'f, M> {
    cx: &'f Cx,
    made: M,
}

impl<'g> Given<'g> {
    /// The start of the check of `main`'s parameters: nothing made yet.
    pub fn checking(&self) -> Checking<'_, 'g, ()> {
        Checking { given: self, made: Ok(()) }
    }
}

impl<'c, 'g, M> Checking<'c, 'g, M> {
    /// The parameter `name` of type `T`, made from the spec and the worlds: kept when it was
    /// made, its problem kept when it was not. Made once: the value kept here is the one `main`
    /// is handed.
    pub fn check<T: Grant>(self, name: &'static str) -> Checking<'c, 'g, (M, T)> {
        let Checking { given, made } = self;
        let this = T::grant(&Granting { cx: given.cx, name, member: None, given });
        let made = match (made, this) {
            (Ok(held), Ok(this)) => Ok((held, this)),
            (Ok(held), Err(want)) => {
                // The start is refused: what was made is unmade now, in the order it was made.
                drop(held);
                let mut problems = Vec::new_in(given.spec.alloc());
                // alloc: a refusal's problems, once per refused start.
                problems.push(Problem::Param { name, kind: T::KIND, want });
                Err(problems)
            }
            (Err(problems), Ok(this)) => {
                // Made only as its check: an earlier parameter refused the start.
                drop(this);
                Err(problems)
            }
            (Err(mut problems), Err(want)) => {
                // alloc: a refusal's problems, once per refused start.
                problems.push(Problem::Param { name, kind: T::KIND, want });
                Err(problems)
            }
        };
        Checking { given, made }
    }

    /// A value made outside the launcher's grants (a test's drawn value, made later from its
    /// tape: `()` here), kept in this parameter's place so the made tuple keeps written order.
    pub fn beside<V>(self, value: V) -> Checking<'c, 'g, (M, V)> {
        let Checking { given, made } = self;
        Checking { given, made: made.map(|held| (held, value)) }
    }

    /// Every parameter was made and the spec grants nothing `main` does not take: the proof
    /// that hands the made values out; else every problem, the parameters' first, in order.
    pub fn filled(self) -> Result<Filled<'c, M>, Refused> {
        let Checking { given, made } = self;
        let alloc = given.spec.alloc();
        let mut unasked: Vec<Problem, LaunchAlloc> = Vec::new_in(alloc);
        for granted in given.spec.get().grants.iter() {
            if given.manifest.taker(granted.name.as_str()).is_none() {
                // clone: a grant's name, for its problem.
                let name = String::from_str_in(granted.name.as_str(), alloc);
                // alloc: a refusal's problems, once per refused start.
                unasked.push(Problem::NoParam { name, given: granted.scope.kind_name() });
            }
        }
        match (made, unasked.is_empty()) {
            (Ok(made), true) => Ok(Filled { cx: given.cx, made }),
            (Ok(made), false) => {
                // Refused by a grant nobody takes: every made parameter is unmade, in order.
                drop(made);
                Err(Refused { problems: unasked, started: given.started })
            }
            (Err(mut problems), _) => {
                problems.append(&mut unasked);
                Err(Refused { problems, started: given.started })
            }
        }
    }
}

impl<'f, M> Filled<'f, M> {
    /// The context `main` runs in.
    pub fn cx(&self) -> &'f Cx {
        self.cx
    }

    /// The values the checks made, in written order, as the left-nested tuple
    /// [`__made!`](crate::__made) matches.
    pub fn hand_out(self) -> M {
        self.made
    }
}

/// The pattern that takes the values [`Filled::hand_out`] gives, by the parameters' names in
/// written order: `__made!(a, b)` is `(((), a), b)`; `__made!()` is `()`. One step per name.
#[doc(hidden)]
#[macro_export]
macro_rules! __made {
    (@ $held:tt ;) => { $held };
    (@ $held:tt ; $name:tt $(, $rest:tt)*) => { $crate::__made!(@ ($held, $name) ; $($rest),*) };
    ($($name:tt),* $(,)?) => { $crate::__made!(@ () ; $($name),*) };
}

#[cfg(test)]
mod tests;
