//! The expression graph: scalar float expressions as a hash-consed DAG.
//!
//! A [`Graph`] is over one float width ([`Width`]). A node ([`Node`]) is an operation, up to
//! three arguments by [`Id`] and a payload (a constant's bits, a variable's index, a `powi`
//! exponent). Adding a node that is there already gives its id, so equal subterms are one node
//! and a term is a DAG. Ids are given in the order nodes are first added, and an argument's id
//! is always below its user's: the nodes in id order are a topological order, and no order here
//! comes from a hash map (the index is only looked up).
//!
//! Integers appear only as a `powi` exponent or a variable's index; a condition is a node of
//! sort [`Sort::Bool`] (a comparison of two floats, or `and`, `or`, `not` of conditions), which
//! [`Op::Select`] chooses by.

use std::alloc::Global;

use blaze_hash::HashMap;

/// The float type a graph computes in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Width {
    F32,
    F64,
}

impl Width {
    /// The type's name in Rust source.
    pub const fn name(self) -> &'static str {
        match self {
            Width::F32 => "f32",
            Width::F64 => "f64",
        }
    }
}

/// A node of a [`Graph`], by the order it was first added.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Id(pub u32);

impl Id {
    /// An argument slot a node does not use.
    pub const NONE: Id = Id(u32::MAX);

    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a node's value is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sort {
    Float,
    Bool,
}

/// A node's operation. The semantics of each is [`crate::eval::apply`]'s alone: Rust's
/// operator or `f32`/`f64` method of the same name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Op {
    /// A float constant: the payload is its bits (an `f32`'s in the low half).
    Const,
    /// A function parameter: the payload is its index.
    Var,
    /// `true` or `false`: the payload is 1 or 0.
    Bool,
    Add,
    Sub,
    Mul,
    Div,
    Neg,
    /// `x.powi(n)`: the payload is `n` (an `i32`).
    Powi,
    Powf,
    Sqrt,
    Exp,
    Ln,
    Sin,
    Cos,
    Tan,
    Tanh,
    Abs,
    /// `a.min(b)`, as `f64::min`: the other operand when one is NaN.
    Min,
    /// `a.max(b)`, as `f64::max`.
    Max,
    /// `a.mul_add(b, c)`: `a * b + c` rounded once.
    MulAdd,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Not,
    /// `if c { a } else { b }`: the first argument is the condition.
    Select,
}

impl Op {
    /// Every operation, in declaration order.
    pub const ALL: &[Op] = &[
        Op::Const,
        Op::Var,
        Op::Bool,
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Div,
        Op::Neg,
        Op::Powi,
        Op::Powf,
        Op::Sqrt,
        Op::Exp,
        Op::Ln,
        Op::Sin,
        Op::Cos,
        Op::Tan,
        Op::Tanh,
        Op::Abs,
        Op::Min,
        Op::Max,
        Op::MulAdd,
        Op::Lt,
        Op::Le,
        Op::Gt,
        Op::Ge,
        Op::Eq,
        Op::Ne,
        Op::And,
        Op::Or,
        Op::Not,
        Op::Select,
    ];

    /// How many arguments a node of it has.
    pub const fn arity(self) -> usize {
        match self {
            Op::Const | Op::Var | Op::Bool => 0,
            Op::Neg
            | Op::Powi
            | Op::Sqrt
            | Op::Exp
            | Op::Ln
            | Op::Sin
            | Op::Cos
            | Op::Tan
            | Op::Tanh
            | Op::Abs
            | Op::Not => 1,
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Powf
            | Op::Min
            | Op::Max
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::Eq
            | Op::Ne
            | Op::And
            | Op::Or => 2,
            Op::MulAdd | Op::Select => 3,
        }
    }

    /// The sort of a node of it.
    pub const fn sort(self) -> Sort {
        match self {
            Op::Bool
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::Eq
            | Op::Ne
            | Op::And
            | Op::Or
            | Op::Not => Sort::Bool,
            _ => Sort::Float,
        }
    }

    /// The sort its argument `at` must have.
    pub const fn arg_sort(self, at: usize) -> Sort {
        match self {
            Op::And | Op::Or | Op::Not => Sort::Bool,
            Op::Select if at == 0 => Sort::Bool,
            _ => Sort::Float,
        }
    }

    /// The operation's name: the `f64` method's, or a word for an operator.
    pub const fn name(self) -> &'static str {
        match self {
            Op::Const => "const",
            Op::Var => "var",
            Op::Bool => "bool",
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::Div => "div",
            Op::Neg => "neg",
            Op::Powi => "powi",
            Op::Powf => "powf",
            Op::Sqrt => "sqrt",
            Op::Exp => "exp",
            Op::Ln => "ln",
            Op::Sin => "sin",
            Op::Cos => "cos",
            Op::Tan => "tan",
            Op::Tanh => "tanh",
            Op::Abs => "abs",
            Op::Min => "min",
            Op::Max => "max",
            Op::MulAdd => "mul_add",
            Op::Lt => "lt",
            Op::Le => "le",
            Op::Gt => "gt",
            Op::Ge => "ge",
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::And => "and",
            Op::Or => "or",
            Op::Not => "not",
            Op::Select => "select",
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One node: an operation, its arguments (unused slots are [`Id::NONE`]) and its payload (0
/// where the operation has none).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Node {
    pub op: Op,
    pub args: [Id; 3],
    pub payload: u64,
}

impl Node {
    /// The arguments the operation has.
    #[inline]
    pub fn args(&self) -> &[Id] {
        &self.args[..self.op.arity()]
    }

    /// A `powi` node's exponent.
    #[inline]
    pub const fn exponent(&self) -> i32 {
        self.payload as u32 as i32
    }
}

/// The payload of a `powi` of exponent `n`.
#[inline]
pub const fn exponent_payload(n: i32) -> u64 {
    n as u32 as u64
}

/// The payload of the constant `value` rounded to `width`: its bits.
#[inline]
pub fn payload_of(width: Width, value: f64) -> u64 {
    match width {
        Width::F32 => u64::from((value as f32).to_bits()),
        Width::F64 => value.to_bits(),
    }
}

/// A node could not be added: an argument is not of the graph, or is of the wrong sort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Argument `at` of an `op` node names no node of the graph.
    NoSuchArg { op: Op, at: usize },
    /// Argument `at` of an `op` node is a `found`, and the operation takes the other sort there.
    Sort { op: Op, at: usize, found: Sort },
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Malformed::NoSuchArg { op, at } => {
                write!(f, "argument {at} of `{op}` is not a node of this graph")
            }
            Malformed::Sort { op, at, found } => {
                write!(
                    f,
                    "argument {at} of `{op}` is a {found:?}, which `{op}` does not take there"
                )
            }
        }
    }
}

/// A hash-consed expression DAG over one float width.
pub struct Graph {
    width: Width,
    /// The nodes, by id.
    nodes: Vec<Node>,
    /// Each node's id: looked up when a node is added, never iterated.
    index: HashMap<Node, Id, Global>,
}

impl Graph {
    pub fn new(width: Width) -> Graph {
        // alloc: the graph owns its nodes; it lives as long as the derivative it holds
        Graph { width, nodes: Vec::new(), index: HashMap::new_in(Global) }
    }

    #[inline]
    pub fn width(&self) -> Width {
        self.width
    }

    /// How many nodes there are: every id is below it.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node `id`. Panics when `id` is not of this graph: an id is only ever one a graph gave.
    #[inline]
    pub fn node(&self, id: Id) -> Node {
        self.nodes[id.index()]
    }

    /// The nodes by id: a topological order, arguments first.
    #[inline]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    #[inline]
    pub fn sort(&self, id: Id) -> Sort {
        self.node(id).op.sort()
    }

    /// The id of `node`, added when it is new. Unused argument slots and an unused payload are
    /// normalized, so equal nodes are one node however they were written.
    pub fn try_add(&mut self, mut node: Node) -> Result<Id, Malformed> {
        let arity = node.op.arity();
        for at in 0..3 {
            if at >= arity {
                node.args[at] = Id::NONE;
                continue;
            }
            let arg = node.args[at];
            let Some(found) = self.nodes.get(arg.index()) else {
                return Err(Malformed::NoSuchArg { op: node.op, at });
            };
            let found = found.op.sort();
            if found != node.op.arg_sort(at) {
                return Err(Malformed::Sort { op: node.op, at, found });
            }
        }
        if !matches!(node.op, Op::Const | Op::Var | Op::Bool | Op::Powi) {
            node.payload = 0;
        }
        if let Some(&id) = self.index.get(&node) {
            return Ok(id);
        }
        let id = Id(u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes"));
        self.nodes.push(node);
        self.index.insert(node, id);
        Ok(id)
    }

    /// [`Graph::try_add`] of a node whose arguments the caller made in this graph with the
    /// right sorts. Panics otherwise: that is a bug in the caller, not an input.
    #[track_caller]
    pub fn add(&mut self, node: Node) -> Id {
        match self.try_add(node) {
            Ok(id) => id,
            Err(error) => panic!("blaze-diff: {error}"),
        }
    }

    /// The constant with these bits (an `f32`'s in the low half for [`Width::F32`]).
    pub fn const_bits(&mut self, bits: u64) -> Id {
        let bits = match self.width {
            Width::F32 => bits & u64::from(u32::MAX),
            Width::F64 => bits,
        };
        self.add(Node { op: Op::Const, args: [Id::NONE; 3], payload: bits })
    }

    /// The constant `value`, rounded to the graph's width.
    pub fn constant(&mut self, value: f64) -> Id {
        self.const_bits(payload_of(self.width, value))
    }

    /// The function's parameter `index`.
    pub fn var(&mut self, index: u32) -> Id {
        self.add(Node { op: Op::Var, args: [Id::NONE; 3], payload: u64::from(index) })
    }

    pub fn bool(&mut self, value: bool) -> Id {
        self.add(Node { op: Op::Bool, args: [Id::NONE; 3], payload: u64::from(value) })
    }

    #[track_caller]
    pub fn unary(&mut self, op: Op, a: Id) -> Id {
        debug_assert!(op.arity() == 1 && !matches!(op, Op::Powi));
        self.add(Node { op, args: [a, Id::NONE, Id::NONE], payload: 0 })
    }

    #[track_caller]
    pub fn binary(&mut self, op: Op, a: Id, b: Id) -> Id {
        debug_assert!(op.arity() == 2);
        self.add(Node { op, args: [a, b, Id::NONE], payload: 0 })
    }

    #[track_caller]
    pub fn powi(&mut self, a: Id, n: i32) -> Id {
        self.add(Node { op: Op::Powi, args: [a, Id::NONE, Id::NONE], payload: exponent_payload(n) })
    }

    #[track_caller]
    pub fn mul_add(&mut self, a: Id, b: Id, c: Id) -> Id {
        self.add(Node { op: Op::MulAdd, args: [a, b, c], payload: 0 })
    }

    #[track_caller]
    pub fn select(&mut self, cond: Id, a: Id, b: Id) -> Id {
        self.add(Node { op: Op::Select, args: [cond, a, b], payload: 0 })
    }

    /// The bits of the constant `id` is, when it is one.
    pub fn as_const_bits(&self, id: Id) -> Option<u64> {
        let node = self.node(id);
        (node.op == Op::Const).then_some(node.payload)
    }

    /// The value of the constant `id` is, as an `f64` (exact for an `f32`).
    pub fn as_const(&self, id: Id) -> Option<f64> {
        let bits = self.as_const_bits(id)?;
        Some(match self.width {
            Width::F32 => f64::from(f32::from_bits(bits as u32)),
            Width::F64 => f64::from_bits(bits),
        })
    }

    /// The index of the variable `id` is, when it is one.
    pub fn as_var(&self, id: Id) -> Option<u32> {
        let node = self.node(id);
        (node.op == Op::Var).then_some(node.payload as u32)
    }

    /// Which nodes `roots` reach, by id: a root, or an argument of one that is reached.
    pub fn reached(&self, roots: &[Id]) -> Vec<bool> {
        // alloc: one flag per node, the caller's scratch
        let mut reached = vec![false; self.nodes.len()];
        for root in roots {
            reached[root.index()] = true;
        }
        // An argument's id is below its user's: one pass from the top marks everything.
        for at in (0..self.nodes.len()).rev() {
            if reached[at] {
                for arg in self.nodes[at].args() {
                    reached[arg.index()] = true;
                }
            }
        }
        reached
    }

    /// How many variables `roots` name: one more than the highest index, 0 without any.
    pub fn arity(&self, roots: &[Id]) -> u32 {
        let reached = self.reached(roots);
        let mut arity = 0;
        for (at, node) in self.nodes.iter().enumerate() {
            if reached[at] && node.op == Op::Var {
                arity = arity.max(node.payload as u32 + 1);
            }
        }
        arity
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Graph({}, {} nodes)", self.width.name(), self.nodes.len())?;
        for (at, node) in self.nodes.iter().enumerate() {
            write!(f, "  n{at} = {}", node.op)?;
            match node.op {
                Op::Const | Op::Var | Op::Bool => write!(f, " {:#x}", node.payload)?,
                Op::Powi => write!(f, " ^{}", node.exponent())?,
                _ => {}
            }
            for arg in node.args() {
                write!(f, " n{}", arg.0)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}
