//! Snapshotting in the infer ctxt of the next-trait-solver.
//!
//! Every table keeps its own undo rows, so a snapshot is where each table stood when it
//! started: lengths of the columns and positions in the undo rows. Rollback undoes each table
//! back to its mark; the tables are independent, so the order they are undone in does not
//! matter. Committing the outermost snapshot drops every undo row.

use crate::next_solver::infer::InferCtxtInner;
use crate::next_solver::infer::opaque_types::OpaqueTypeMark;
use crate::next_solver::infer::region_constraints::RegionSnapshot;
use crate::next_solver::infer::type_variable::TypeVariableMark;
use crate::next_solver::infer::union_find::UnionFindMark;

pub struct Snapshot {
    pub(crate) type_variables: TypeVariableMark,
    pub(crate) consts: UnionFindMark,
    pub(crate) ints: UnionFindMark,
    pub(crate) floats: UnionFindMark,
    pub(crate) region_constraints: RegionSnapshot,
    pub(crate) opaque_types: OpaqueTypeMark,
    pub(crate) region_obligations: u32,
    pub(crate) region_assumptions: u32,
}

/// How many snapshots are open; the undo rows themselves are in each table.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct InferCtxtUndoLogs {
    num_open_snapshots: usize,
}

impl InferCtxtUndoLogs {
    #[inline]
    pub(crate) fn in_snapshot(&self) -> bool {
        self.num_open_snapshots > 0
    }

    #[inline]
    pub(crate) fn num_open_snapshots(&self) -> usize {
        self.num_open_snapshots
    }
}

impl<'db> InferCtxtInner<'db> {
    pub(crate) fn start_snapshot(&mut self) -> Snapshot {
        self.undo_log.num_open_snapshots += 1;
        Snapshot {
            type_variables: self.type_variable_storage.start_snapshot(),
            consts: self.const_unification_storage.start_snapshot(),
            ints: self.int_unification_storage.start_snapshot(),
            floats: self.float_unification_storage.start_snapshot(),
            region_constraints: self
                .region_constraint_storage
                .as_mut()
                .expect("region constraints already solved")
                .start_snapshot(),
            opaque_types: self.opaque_type_storage.start_snapshot(),
            region_obligations: self.region_obligations.len() as u32,
            region_assumptions: self.region_assumptions.len() as u32,
        }
    }

    pub fn rollback_to(&mut self, snapshot: Snapshot) {
        assert!(self.undo_log.num_open_snapshots > 0, "rolled back with no open snapshot");

        self.type_variable_storage.rollback_to(snapshot.type_variables);
        self.const_unification_storage.rollback_to(snapshot.consts);
        let consts = self.const_unification_storage.len();
        while self.const_var_params.last().is_some_and(|&(vid, _)| vid.as_usize() >= consts) {
            self.const_var_params.pop();
        }
        self.int_unification_storage.rollback_to(snapshot.ints);
        self.float_unification_storage.rollback_to(snapshot.floats);
        if let Some(region_constraints) = self.region_constraint_storage.as_mut() {
            region_constraints.rollback_to(snapshot.region_constraints);
        }
        self.opaque_type_storage.rollback_to(snapshot.opaque_types);
        let obligations = snapshot.region_obligations as usize;
        assert!(
            self.region_obligations.len() >= obligations,
            "pushed region constraint but could not pop it"
        );
        self.region_obligations.truncate(obligations);
        let assumptions = snapshot.region_assumptions as usize;
        assert!(
            self.region_assumptions.len() >= assumptions,
            "pushed region assumption but could not pop it"
        );
        self.region_assumptions.truncate(assumptions);

        self.undo_log.num_open_snapshots -= 1;
    }

    pub fn commit(&mut self, snapshot: Snapshot) {
        assert!(self.undo_log.num_open_snapshots > 0, "committed with no open snapshot");
        self.undo_log.num_open_snapshots -= 1;
        // The outermost snapshot: there is no snapshot further out that we might need to roll
        // back to, so the undo rows can go.
        let outermost = self.undo_log.num_open_snapshots == 0;

        self.type_variable_storage.commit(snapshot.type_variables);
        self.const_unification_storage.commit(snapshot.consts);
        self.int_unification_storage.commit(snapshot.ints);
        self.float_unification_storage.commit(snapshot.floats);
        if let Some(region_constraints) = self.region_constraint_storage.as_mut() {
            region_constraints.commit(snapshot.region_constraints, outermost);
        }
        self.opaque_type_storage.commit(outermost);
    }
}
