//! Reduced from derived metadata: a const constructor's borrowed array mixes associated
//! constants and const calls. Type inference resolves the constants that name lookup misses.

#[derive(Clone, Copy)]
enum Node {
    Leaf(u8),
    Group(&'static [Node]),
}

impl Node {
    const fn leaf(value: u8) -> Self {
        Self::Leaf(value)
    }

    const fn group(children: &'static [Self]) -> Self {
        Self::Group(children)
    }

    fn sum(self) -> u32 {
        match self {
            Self::Leaf(value) => u32::from(value),
            Self::Group(children) => children.iter().map(|child| child.sum()).sum(),
        }
    }
}

trait HasTree {
    const TREE: Node;
    const EXTENDED: Node = Node::group(&[Self::TREE, Node::leaf(4)]);
}

struct Base;

impl HasTree for Base {
    const TREE: Node = Node::group(&[Node::leaf(1), Node::leaf(2)]);
}

struct Combined;

impl HasTree for Combined {
    const TREE: Node = Node::group(&[<Base as HasTree>::TREE, Node::leaf(3)]);
}

impl Combined {
    const INHERENT: Node = Node::group(&[Base::TREE, Node::leaf(5)]);
}

pub fn run() -> (u32, u32, u32) {
    (Combined::TREE.sum(), Base::EXTENDED.sum(), Combined::INHERENT.sum())
}
