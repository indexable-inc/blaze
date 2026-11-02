//! Snapshotting in the infer ctxt of the next-trait-solver.

use rustc_type_ir::UniverseIndex;

use super::InferCtxt;

mod fudge;
pub(crate) mod undo_log;

use undo_log::Snapshot;

#[must_use = "once you start a snapshot, you should always consume it"]
pub struct CombinedSnapshot {
    pub(super) undo_snapshot: Snapshot,
    universe: UniverseIndex,
}

struct VariableLengths {
    region_constraints_len: usize,
    type_var_len: usize,
    int_var_len: usize,
    float_var_len: usize,
    const_var_len: usize,
}

impl<'db> InferCtxt<'db> {
    fn variable_lengths(&self) -> VariableLengths {
        let mut inner = self.inner.borrow_mut();
        VariableLengths {
            region_constraints_len: inner.unwrap_region_constraints().num_region_vars(),
            type_var_len: inner.type_variables().num_vars(),
            int_var_len: inner.int_unification_table().len(),
            float_var_len: inner.float_unification_table().len(),
            const_var_len: inner.const_unification_table().len(),
        }
    }

    pub fn in_snapshot(&self) -> bool {
        self.inner.borrow().undo_log.in_snapshot()
    }

    pub fn num_open_snapshots(&self) -> usize {
        self.inner.borrow().undo_log.num_open_snapshots()
    }

    pub fn start_snapshot(&self) -> CombinedSnapshot {
        let mut inner = self.inner.borrow_mut();

        CombinedSnapshot { undo_snapshot: inner.start_snapshot(), universe: self.universe() }
    }

    pub fn rollback_to(&self, snapshot: CombinedSnapshot) {
        let CombinedSnapshot { undo_snapshot, universe } = snapshot;

        self.universe.set(universe);

        self.inner.borrow_mut().rollback_to(undo_snapshot);
    }

    pub fn commit_from(&self, snapshot: CombinedSnapshot) {
        let CombinedSnapshot { undo_snapshot, universe: _ } = snapshot;

        self.inner.borrow_mut().commit(undo_snapshot);
    }

    /// Execute `f` and commit the bindings if closure `f` returns `Ok(_)`.
    pub fn commit_if_ok<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce(&CombinedSnapshot) -> Result<T, E>,
    {
        let snapshot = self.start_snapshot();
        let r = f(&snapshot);
        match r {
            Ok(_) => {
                self.commit_from(snapshot);
            }
            Err(_) => {
                self.rollback_to(snapshot);
            }
        }
        r
    }

    /// Execute `f` then unroll any bindings it creates.
    pub fn probe<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&CombinedSnapshot) -> R,
    {
        let snapshot = self.start_snapshot();
        let r = f(&snapshot);
        self.rollback_to(snapshot);
        r
    }

    /// Scan the constraints produced since `snapshot` and check whether
    /// we added any region constraints.
    pub fn region_constraints_added_in_snapshot(&self, snapshot: &CombinedSnapshot) -> bool {
        self.inner
            .borrow_mut()
            .unwrap_region_constraints()
            .region_constraints_added_in_snapshot(&snapshot.undo_snapshot.region_constraints)
    }

    pub fn opaque_types_added_in_snapshot(&self, snapshot: &CombinedSnapshot) -> bool {
        self.inner
            .borrow()
            .opaque_type_storage
            .registered_since(&snapshot.undo_snapshot.opaque_types)
    }
}
