//! The traversal orders of an access over a tree. Miri: `tree_borrows/tree_visitor.rs`. The
//! explicit stack is a buffer the tree keeps between accesses, not a fresh `Vec` per visit.

use super::{
    tree::{AccessRelatedness, Nodes},
    unimap::UniIndex,
};

/// What the per-node functions see.
pub(super) struct NodeAppArgs<'visit, T> {
    pub(super) idx: UniIndex,
    pub(super) rel_pos: AccessRelatedness,
    pub(super) nodes: &'visit mut Nodes,
    pub(super) data: &'visit mut T,
}

/// Whether to go on into a node's children.
#[derive(Debug)]
pub(super) enum ContinueTraversal {
    Recurse,
    SkipSelfAndChildren,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ChildrenVisitMode {
    VisitChildrenOfAccessed,
    SkipChildrenOfAccessed,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum RecursionState {
    BeforeChildren,
    AfterChildren,
}

pub(super) type VisitStack = Vec<(UniIndex, AccessRelatedness, RecursionState)>;

/// The parts of a tree a traversal needs. Children and parents must not change during it.
pub(super) struct TreeVisitor<'tree, T> {
    pub(super) nodes: &'tree mut Nodes,
    pub(super) data: &'tree mut T,
    pub(super) stack: &'tree mut VisitStack,
}

struct Visit<'tree, 'v, T, NodeContinue, NodeApp> {
    this: &'v mut TreeVisitor<'tree, T>,
    f_continue: NodeContinue,
    f_propagate: NodeApp,
}

impl<T, NodeContinue, NodeApp, Err> Visit<'_, '_, T, NodeContinue, NodeApp>
where
    NodeContinue: Fn(&NodeAppArgs<'_, T>) -> ContinueTraversal,
    NodeApp: FnMut(NodeAppArgs<'_, T>) -> Result<(), Err>,
{
    fn should_continue_at(&mut self, idx: UniIndex, rel_pos: AccessRelatedness) -> ContinueTraversal {
        let args = NodeAppArgs { idx, rel_pos, nodes: self.this.nodes, data: self.this.data };
        (self.f_continue)(&args)
    }

    fn propagate_at(&mut self, idx: UniIndex, rel_pos: AccessRelatedness) -> Result<(), Err> {
        (self.f_propagate)(NodeAppArgs { idx, rel_pos, nodes: self.this.nodes, data: self.this.data })
    }

    /// The accessed node, then its parents (all local), pushing the cousins (and, unless
    /// skipped, the accessed node's children) for later; returns the root. Miri:
    /// `TreeVisitorStack::go_upwards_from_accessed`.
    fn go_upwards_from_accessed(
        &mut self,
        accessed_node: UniIndex,
        visit_children: ChildrenVisitMode,
    ) -> Result<UniIndex, Err> {
        self.this.stack.clear();
        self.propagate_at(accessed_node, AccessRelatedness::LocalAccess)?;
        if matches!(visit_children, ChildrenVisitMode::VisitChildrenOfAccessed) {
            for &child in self.this.nodes.children(accessed_node).iter().rev() {
                self.this.stack.push((
                    child,
                    AccessRelatedness::ForeignAccess,
                    RecursionState::BeforeChildren,
                ));
            }
        }
        let mut last_node = accessed_node;
        while let Some(current) = self.this.nodes.parent(last_node) {
            self.propagate_at(current, AccessRelatedness::LocalAccess)?;
            for &child in self.this.nodes.children(current).iter().rev() {
                if last_node == child {
                    continue;
                }
                self.this.stack.push((
                    child,
                    AccessRelatedness::ForeignAccess,
                    RecursionState::BeforeChildren,
                ));
            }
            last_node = current;
        }
        self.this.stack.reverse();
        Ok(last_node)
    }

    /// Bottom-up over what is on the stack, letting `f_continue` skip subtrees. Miri:
    /// `TreeVisitorStack::finish_foreign_accesses`.
    fn finish_foreign_accesses(&mut self) -> Result<(), Err> {
        while let Some(&(idx, rel_pos, step)) = self.this.stack.last() {
            match step {
                RecursionState::BeforeChildren => {
                    self.this.stack.last_mut().unwrap().2 = RecursionState::AfterChildren;
                    match self.should_continue_at(idx, rel_pos) {
                        ContinueTraversal::Recurse => {
                            let TreeVisitor { nodes, stack, .. } = &mut *self.this;
                            for &child in nodes.children(idx).iter() {
                                stack.push((child, rel_pos, RecursionState::BeforeChildren));
                            }
                        }
                        ContinueTraversal::SkipSelfAndChildren => {
                            self.this.stack.pop();
                        }
                    }
                }
                RecursionState::AfterChildren => {
                    self.this.stack.pop();
                    self.propagate_at(idx, rel_pos)?;
                }
            }
        }
        Ok(())
    }
}

impl<'tree, T> TreeVisitor<'tree, T> {
    /// Applies `f_propagate` to every node piecewise bottom-up: first `start_idx` and its
    /// ancestors, then the children of `start_idx`, then the rest, bottom-up in each part, so
    /// errors of insufficient permissions closest to the access come first, then protector
    /// violations. `f_continue` may skip a foreign subtree. Returns the root of the tree.
    ///
    /// ```text
    ///         3
    ///        /|
    ///       / |
    ///      9  2
    ///      |  |\
    ///      |  | \
    ///      8  1  7
    ///        / \
    ///       4   6
    ///           |
    ///           5
    /// ```
    ///
    /// Miri: `TreeVisitor::traverse_this_parents_children_other`.
    pub(super) fn traverse_this_parents_children_other<Err>(
        mut self,
        start_idx: UniIndex,
        f_continue: impl Fn(&NodeAppArgs<'_, T>) -> ContinueTraversal,
        f_propagate: impl FnMut(NodeAppArgs<'_, T>) -> Result<(), Err>,
    ) -> Result<UniIndex, Err> {
        let mut visit = Visit { this: &mut self, f_continue, f_propagate };
        let root =
            visit.go_upwards_from_accessed(start_idx, ChildrenVisitMode::VisitChildrenOfAccessed)?;
        visit.finish_foreign_accesses()?;
        Ok(root)
    }

    /// Like `traverse_this_parents_children_other` without the children of `start_idx`.
    /// Miri: `TreeVisitor::traverse_nonchildren`.
    pub(super) fn traverse_nonchildren<Err>(
        mut self,
        start_idx: UniIndex,
        f_continue: impl Fn(&NodeAppArgs<'_, T>) -> ContinueTraversal,
        f_propagate: impl FnMut(NodeAppArgs<'_, T>) -> Result<(), Err>,
    ) -> Result<UniIndex, Err> {
        let mut visit = Visit { this: &mut self, f_continue, f_propagate };
        let root =
            visit.go_upwards_from_accessed(start_idx, ChildrenVisitMode::SkipChildrenOfAccessed)?;
        visit.finish_foreign_accesses()?;
        Ok(root)
    }

    /// `start_idx` and all its children, children before parents, each as a foreign access
    /// for `f_continue`. Miri: `TreeVisitor::traverse_children_this`.
    pub(super) fn traverse_children_this<Err>(
        mut self,
        start_idx: UniIndex,
        f_continue: impl Fn(&NodeAppArgs<'_, T>) -> ContinueTraversal,
        f_propagate: impl FnMut(NodeAppArgs<'_, T>) -> Result<(), Err>,
    ) -> Result<(), Err> {
        self.stack.clear();
        self.stack.push((start_idx, AccessRelatedness::ForeignAccess, RecursionState::BeforeChildren));
        let mut visit = Visit { this: &mut self, f_continue, f_propagate };
        visit.finish_foreign_accesses()
    }
}
