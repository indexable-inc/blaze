//! `function_weight` and `deep_nesting`: cognitive complexity by G. Ann Campbell's rules
//! ("Cognitive Complexity: a new way of measuring understandability", SonarSource, 2017,
//! revised 2018; section 3, "Basic criteria and methodology", and appendix B, the
//! specification), read on the syntax the reader reads, not on HIR, whose `for`, `while` and
//! `?` are desugared into `loop`s and `match`es the reader never wrote.
//!
//! Increments (B1): `if`, `else if`, `else`, `match` (Campbell's `switch`), `for`, `while`,
//! `loop` (and `while let`), a `let .. else` (a conditional branch the reader reads as an `if`;
//! the paper predates it), each labelled `break` or `continue` (a jump to a label), each
//! sequence of like boolean operators (`a && b && c` is one, `a && b || c` two: the operators
//! of one expression in source order, parentheses read through, a change of operator starting
//! a new sequence; a `!` starts a new expression), and a function that calls itself (+1 once,
//! found by name resolution, given by the caller).
//!
//! Nesting increments (B2): `if`, `match`, a loop and a `let .. else` add the nesting they are
//! at; `else if` and `else` add none (B3, "hybrid" increments). Nesting levels (B3): the bodies
//! of `if`, `else if`, `else`, `match` arms, loops, `let .. else` blocks and closures (Campbell's
//! lambdas). No increment: `?`, `return`, an unlabelled `break`/`continue`, a macro call (its
//! arguments are tokens, not code the walk can read), a nested item (a function is weighed on
//! its own).
//!
//! `deep_nesting`'s depth is of control flow only (Sonar's S134 counts the same structures):
//! the most `if`/`match`/loop/`let .. else` bodies one point is inside; an `else if` chain is
//! one level, and a closure adds none.

use std::alloc::Global;

use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, BinaryOp, LogicOp},
};

/// A structure of the body and what it adds, everything inside it included: a candidate to
/// move into its own function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) range: TextRange,
    /// What it is: `if`, `match`, `match arm`, `loop`, `closure`, `let-else`.
    pub(crate) kind: &'static str,
    pub(crate) weight: u64,
}

/// A body's weight, its depth and its parts.
#[derive(Debug, Default)]
pub(crate) struct Weight {
    pub(crate) total: u64,
    pub(crate) depth: u32,
    /// The structure that is deepest, first met.
    pub(crate) deepest: Option<(TextRange, &'static str)>,
    pub(crate) parts: Vec<Part, Global>,
}

/// How deep in the tree the walk goes before it stops descending: past any code a person
/// writes, it keeps a degenerate tree (a generated chain of thousands of `+`) off the stack.
const MOST_DEPTH: u32 = 400;

/// The weight of `body`, a function's block, without the recursion increment.
pub(crate) fn weigh(body: ast::Node<'_>) -> Weight {
    let mut walk = Walk {
        weight: Weight { parts: Vec::new_in(Global), ..Weight::default() },
    };
    walk.node(body, At { nesting: 0, level: 0, tree: 0 });
    walk.weight
}

/// Where a node is: its nesting (for weight), its control-flow level (for depth) and its depth
/// in the tree (for [`MOST_DEPTH`]).
#[derive(Clone, Copy)]
struct At {
    nesting: u64,
    level: u32,
    tree: u32,
}

impl At {
    fn child(self) -> At {
        At { tree: self.tree + 1, ..self }
    }

    /// Inside a structure's body: nested one more, and one level deeper when it is control
    /// flow.
    fn inside(self, control: bool) -> At {
        At { nesting: self.nesting + 1, level: self.level + u32::from(control), tree: self.tree + 1 }
    }
}

struct Walk {
    weight: Weight,
}

impl Walk {
    fn add(&mut self, by: u64) {
        self.weight.total = self.weight.total.saturating_add(by);
    }

    /// Notes that a control-flow body at `level` starts at `node`.
    fn reach(&mut self, level: u32, node: ast::Node<'_>, kind: &'static str) {
        if level > self.weight.depth {
            self.weight.depth = level;
            self.weight.deepest = Some((node.text_range(), kind));
        }
    }

    fn part(&mut self, node: ast::Node<'_>, kind: &'static str, from: u64) {
        let weight = self.weight.total - from;
        if weight > 0 {
            self.weight.parts.push(Part { range: node.text_range(), kind, weight });
        }
    }

    fn children(&mut self, node: ast::Node<'_>, at: At) {
        for child in node.children() {
            self.node(child, at.child());
        }
    }

    fn node(&mut self, node: ast::Node<'_>, at: At) {
        if at.tree > MOST_DEPTH {
            return;
        }
        match node.kind() {
            // A nested item is weighed as its own.
            SyntaxKind::FN
            | SyntaxKind::IMPL
            | SyntaxKind::TRAIT
            | SyntaxKind::MODULE
            | SyntaxKind::CONST
            | SyntaxKind::STATIC
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::UNION
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::MACRO_RULES
            | SyntaxKind::MACRO_DEF
            | SyntaxKind::EXTERN_BLOCK => {}
            SyntaxKind::IF_EXPR => {
                if let Some(it) = ast::IfExpr::cast(node) {
                    self.if_(it, at, false);
                }
            }
            SyntaxKind::MATCH_EXPR => self.match_(node, at),
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::LOOP_EXPR => {
                self.loop_(node, at)
            }
            SyntaxKind::LET_STMT => self.let_(node, at),
            SyntaxKind::CLOSURE_EXPR => {
                let from = self.weight.total;
                self.children(node, at.inside(false));
                self.part(node, "closure", from);
            }
            SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR => {
                if node.children().any(|it| it.kind() == SyntaxKind::LIFETIME) {
                    self.add(1);
                }
                self.children(node, at);
            }
            SyntaxKind::BIN_EXPR => {
                if is_logic(node) && !in_logic(node) {
                    self.add(sequences(node));
                }
                self.children(node, at);
            }
            _ => self.children(node, at),
        }
    }

    /// An `if`, or the `if` of an `else if` (`chained`): its condition at its own nesting, its
    /// branches one deeper.
    fn if_(&mut self, it: ast::IfExpr<'_>, at: At, chained: bool) {
        let from = self.weight.total;
        let node = it.syntax();
        if chained {
            self.add(1);
        } else {
            self.add(1 + at.nesting);
        }
        if let Some(condition) = it.condition() {
            self.node(condition.syntax(), at.child());
        }
        if let Some(then) = it.then_branch() {
            self.reach(at.level + 1, then.syntax(), "if");
            self.node(then.syntax(), at.inside(true));
        }
        match it.else_branch() {
            Some(ast::ElseBranch::IfExpr(next)) => self.if_(next, at, true),
            Some(ast::ElseBranch::Block(block)) => {
                self.add(1);
                self.node(block.syntax(), at.inside(true));
            }
            None => {}
        }
        if !chained {
            self.part(node, "if", from);
        }
    }

    fn match_(&mut self, node: ast::Node<'_>, at: At) {
        let from = self.weight.total;
        self.add(1 + at.nesting);
        for child in node.children() {
            if child.kind() != SyntaxKind::MATCH_ARM_LIST {
                self.node(child, at.child());
                continue;
            }
            self.reach(at.level + 1, child, "match");
            for arm in child.children() {
                let arm_from = self.weight.total;
                self.children(arm, at.inside(true));
                self.part(arm, "match arm", arm_from);
            }
        }
        self.part(node, "match", from);
    }

    /// A loop: its iterable or condition at its own nesting, its body (its last block) one
    /// deeper.
    fn loop_(&mut self, node: ast::Node<'_>, at: At) {
        let from = self.weight.total;
        self.add(1 + at.nesting);
        let body = node.children().filter(|it| it.kind() == SyntaxKind::BLOCK_EXPR).last();
        for child in node.children() {
            if Some(child) == body {
                self.reach(at.level + 1, child, "loop");
                self.node(child, at.inside(true));
            } else {
                self.node(child, at.child());
            }
        }
        self.part(node, "loop", from);
    }

    /// A `let`: a `let .. else` is weighed as an `if` whose `else` diverges.
    fn let_(&mut self, node: ast::Node<'_>, at: At) {
        let Some(otherwise) = node.children().find(|it| it.kind() == SyntaxKind::LET_ELSE) else {
            self.children(node, at);
            return;
        };
        let from = self.weight.total;
        self.add(1 + at.nesting);
        for child in node.children() {
            if child == otherwise {
                self.reach(at.level + 1, child, "let-else");
                self.children(child, at.inside(true));
            } else {
                self.node(child, at.child());
            }
        }
        self.part(node, "let-else", from);
    }
}

fn logic_op(node: ast::Node<'_>) -> Option<LogicOp> {
    match ast::BinExpr::cast(node)?.op_kind()? {
        BinaryOp::LogicOp(op) => Some(op),
        _ => None,
    }
}

fn is_logic(node: ast::Node<'_>) -> bool {
    logic_op(node).is_some()
}

/// Whether `node`, a logical operation, is an operand of another, parentheses read through.
fn in_logic(node: ast::Node<'_>) -> bool {
    let mut up = node.parent();
    while let Some(parent) = up {
        match parent.kind() {
            SyntaxKind::PAREN_EXPR => up = parent.parent(),
            _ => return is_logic(parent),
        }
    }
    false
}

/// The sequences of like operators of the logical expression `node`: its operators in source
/// order, one sequence and one more at each change of operator.
fn sequences(node: ast::Node<'_>) -> u64 {
    let mut ops: Vec<LogicOp, Global> = Vec::new_in(Global);
    operators(node, &mut ops, 0);
    let changes = ops.windows(2).filter(|it| it[0] != it[1]).count() as u64;
    u64::from(!ops.is_empty()) + changes
}

fn operators(node: ast::Node<'_>, out: &mut Vec<LogicOp, Global>, depth: u32) {
    if depth > MOST_DEPTH {
        return;
    }
    if node.kind() == SyntaxKind::PAREN_EXPR {
        for child in node.children() {
            operators(child, out, depth + 1);
        }
        return;
    }
    let Some(op) = logic_op(node) else { return };
    let Some(bin) = ast::BinExpr::cast(node) else { return };
    if let Some(lhs) = bin.lhs() {
        operators(lhs.syntax(), out, depth + 1);
    }
    out.push(op);
    if let Some(rhs) = bin.rhs() {
        operators(rhs.syntax(), out, depth + 1);
    }
}

/// The part to move out that brings `total` to `bound` or under with the least moved: the
/// smallest such part, else the largest part (it brings it nearest), never one that is the
/// whole. Its index in `parts`.
pub(crate) fn way_down(total: u64, bound: u64, parts: &[(TextRange, u64)]) -> Option<usize> {
    let mut fits: Option<usize> = None;
    let mut largest: Option<usize> = None;
    for (at, &(range, score)) in parts.iter().enumerate() {
        if score == 0 || score >= total {
            continue;
        }
        let better = |best: Option<usize>, by: fn(u64, u64) -> bool| match best {
            None => true,
            Some(it) => {
                let (other, held) = parts[it];
                by(score, held) || (score == held && range.start() < other.start())
            }
        };
        if total - score <= bound && better(fits, |a, b| a < b) {
            fits = Some(at);
        }
        if better(largest, |a, b| a > b) {
            largest = Some(at);
        }
    }
    fits.or(largest)
}
