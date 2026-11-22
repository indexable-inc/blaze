//! The record walk's maker over ANY `FrontEnd` (run/make_one.rs `Maker` is the same over the
//! lazy program alone): the first make makes the maker's state from the program, each make
//! is `make_instance`, as `Maker::make_over` does. So the whole program and the lazy one are
//! driven by the same walk, the same maker and the same builder: only the front end differs.
//!
//! A static's value is taken through the seam (`static_item` and `static_records`), never the
//! lazy program's own `static_value`, so both programs answer the same question; the value
//! taken is logged as the question `static_value <key>` (front_equal/asked.rs `note`).

use blaze_build::{OverflowChecks, Panic, UbChecks};
use blaze_emit::Isa;
use blaze_ladder::body_ids::{Fixture, Make};
use blaze_ladder::code_records::Builder;
use blaze_ladder::run::RunError;
use blaze_ladder::run::make_one::{CheckCount, MakerState, SecondCheck, make_instance};
use blaze_ladder::seam::FrontEnd;
use blaze_own::{AVec, Alloc};
use blaze_typeck::ItemKey;

use super::asked::Asked;
use super::shown;

/// One program behind the recording wrapper, as the walk's maker.
pub struct Over<'a, P> {
    pub program: Asked<'a, P>,
    state: Option<MakerState>,
    checks: CheckCount,
    /// The builder's settings as make_one.rs `Maker::new` reads them.
    isa: Isa,
    overflow: OverflowChecks,
    ub: UbChecks,
    panic: Panic,
    alloc: Alloc<'static>,
}

impl<'a, P: FrontEnd> Over<'a, P> {
    pub fn new(program: Asked<'a, P>, builder: &Builder, alloc: Alloc<'static>) -> Over<'a, P> {
        Over {
            program,
            state: None,
            checks: CheckCount::default(),
            isa: builder.isa,
            overflow: builder.settings.overflow_checks,
            ub: UbChecks::of_debug_assertions(builder.settings.debug_assertions),
            panic: blaze_ladder::panic_strategy::of_strategy(builder.settings.panic),
            alloc,
        }
    }

    /// The maker's state, made on the first make from the program (make_one.rs
    /// `Maker::construct`).
    fn ensure(&mut self, first: ItemKey) -> Result<(), RunError> {
        if self.state.is_none() {
            let state = MakerState::new(&self.program, first, self.isa, self.overflow, self.ub, self.panic, self.alloc)?;
            self.state = Some(state);
        }
        Ok(())
    }

    /// Static `item`'s entry from its value as the seam lends it: its datum, or (`template`)
    /// a thread-local's template.
    fn static_entry(&mut self, item: ItemKey, template: bool, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.ensure(item)?;
        let Some(kind) = self.program.static_item(item) else {
            return Err(RunError::OneBody { key: item, why: "the front end names no static of this key (`FrontEnd::static_item`)" });
        };
        let Some((_, value)) = self.program.inner.static_records().iter().find(|(k, _)| *k == item) else {
            return Err(RunError::OneBody { key: item, why: "the front end made no value for this static (`FrontEnd::static_records`)" });
        };
        self.program.note(format_args!("static_value {item:?}"), |o| shown::data(o, value));
        let Some(state) = self.state.as_mut() else {
            return Err(RunError::OneBody { key: item, why: "the maker's state was not made (front_equal/over.rs `ensure`)" });
        };
        match state.static_entry(&self.program, value, kind, template) {
            Ok(made) => {
                entry.extend_from_slice(&made);
                Ok(())
            }
            Err(error) => match self.program.inner.item_named(item) {
                Some(static_) => Err(RunError::StaticRecord { static_, error }),
                None => Err(RunError::OneBody { key: item, why: "the static's value made no entry (code_records.rs `statics`), and the front end names it not" }),
            },
        }
    }
}

impl<P: FrontEnd> Make for Over<'_, P> {
    type Error = RunError;
    type Grant = Fixture;

    fn make(&mut self, _grant: &Fixture, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.ensure(tabled)?;
        let Some(state) = self.state.as_mut() else {
            return Err(RunError::OneBody { key: tabled, why: "the maker's state was not made (front_equal/over.rs `ensure`)" });
        };
        make_instance(&mut self.program, &mut SecondCheck, state, &mut self.checks, tabled, name, entry)
    }

    fn make_static(&mut self, _grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.static_entry(item, false, entry)
    }

    fn make_thread_local(&mut self, _grant: &Fixture, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.static_entry(item, true, entry)
    }
}
