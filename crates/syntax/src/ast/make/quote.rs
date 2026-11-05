//! A `quote!`-like API for crafting AST nodes.

pub(crate) use syntax_tree::{FlatTreeBuilder, SyntaxKind as RSyntaxKind};

macro_rules! quote_impl_ {
    ( @append $builder:ident ) => {}; // Base case.

    ( @append $builder:ident
        $node:ident {
            $($tree:tt)*
        }
        $($rest:tt)*
    ) => {
        {
            let kind = <$crate::ast::$node as $crate::ast::AstNode>::kind();
            $builder.start_node($crate::ast::make::quote::RSyntaxKind(kind as u16));
            $crate::ast::make::quote::quote_impl!( @append $builder
                $($tree)*
            );
            $builder.finish_node();
        }
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    ( @append $builder:ident
        [ $token_kind:ident $token_text:expr ]
        $($rest:tt)*
    ) => {
        $builder.token(
            $crate::ast::make::quote::RSyntaxKind($crate::SyntaxKind::$token_kind as u16),
            &$token_text,
        );
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    ( @append $builder:ident
        [$($token:tt)+]
        $($rest:tt)*
    ) => {
        $builder.token(
            $crate::ast::make::quote::RSyntaxKind($crate::T![ $($token)+ ] as u16),
            const { $crate::T![ $($token)+ ].text() },
        );
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    ( @append $builder:ident
        $whitespace:literal
        $($rest:tt)*
    ) => {
        const { $crate::ast::make::quote::verify_only_whitespaces($whitespace) };
        $builder.token(
            $crate::ast::make::quote::RSyntaxKind($crate::SyntaxKind::WHITESPACE as u16),
            $whitespace,
        );
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    ( @append $builder:ident
        # $var:ident
        $($rest:tt)*
    ) => {
        $crate::ast::make::quote::ToNodeChild::append_node_child($var, &mut $builder);
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    ( @append $builder:ident
        #( $($repetition:tt)+ )*
        $($rest:tt)*
    ) => {
        $crate::ast::make::quote::quote_impl!( @extract_pounded_in_repetition $builder
            [] [] $($repetition)*
        );
        $crate::ast::make::quote::quote_impl!( @append $builder $($rest)* );
    };

    // Base case - no repetition var.
    ( @extract_pounded_in_repetition $builder:ident
        [ $($repetition:tt)* ] [ ]
    ) => {
        ::std::compile_error!("repetition in `ast::make::quote!()` without variable");
    };

    // Base case - repetition var found.
    ( @extract_pounded_in_repetition $builder:ident
        [ $($repetition:tt)* ] [ $repetition_var:ident ]
    ) => {
        ::std::iter::IntoIterator::into_iter($repetition_var).for_each(|$repetition_var| {
            $crate::ast::make::quote::quote_impl!( @append $builder $($repetition)* );
        });
    };

    ( @extract_pounded_in_repetition $builder:ident
        [ $($repetition:tt)* ] [ $repetition_var1:ident ] # $repetition_var2:ident $($rest:tt)*
    ) => {
        ::std::compile_error!("repetition in `ast::make::quote!()` with more than one variable");
    };

    ( @extract_pounded_in_repetition $builder:ident
        [ $($repetition:tt)* ] [ ] # $repetition_var:ident $($rest:tt)*
    ) => {
        $crate::ast::make::quote::quote_impl!( @extract_pounded_in_repetition $builder
            [ $($repetition)* # $repetition_var ] [ $repetition_var ] $($rest)*
        );
    };

    ( @extract_pounded_in_repetition $builder:ident
        [ $($repetition:tt)* ] [ $($repetition_var:tt)* ] $non_repetition_var:tt $($rest:tt)*
    ) => {
        $crate::ast::make::quote::quote_impl!( @extract_pounded_in_repetition $builder
            [ $($repetition)* $non_repetition_var ] [ $($repetition_var)* ] $($rest)*
        );
    };
}
pub(crate) use quote_impl_ as quote_impl;

/// A `quote!`-like API for crafting AST nodes.
///
/// Syntax: AST nodes are created with `Node { children }`, where `Node` is the node name in `ast` (`ast::Node`).
/// Tokens are creates with their syntax enclosed by brackets, e.g. `[::]` or `['{']`. Alternatively, tokens can
/// be created with the syntax `[token_kind token_text]`, where `token_kind` is a variant of `SyntaxKind` (e.g.
/// `IDENT`) and `token_text` is an expression producing `String` or `&str`. Whitespaces can be added
/// as string literals (i.e. `"\n    "` is a whitespace token). Interpolation is allowed with `#` (`#variable`),
/// from `AstNode`s and `Option`s of them. Repetition is also supported, with only one repeating variable
/// and no separator (`#("\n" #variable [>])*`), for any `IntoIterator`. Note that `Option`s are also `IntoIterator`,
/// which can help when you want to conditionally include something along with an optional node.
///
/// There needs to be one root node, and its type is returned. The node is the root of a flat
/// tree of its own, built in one pass: interpolated nodes are copied into it.
///
/// Be careful to closely match the Ungrammar AST, there is no validation for this!
macro_rules! quote_ {
    ( $root:ident { $($tree:tt)* } ) => {{
        let mut builder = $crate::ast::make::quote::FlatTreeBuilder::new();
        $crate::ast::make::quote::quote_impl!( @append builder $root { $($tree)* } );
        let root = $crate::SyntaxNode::new_flat_root(::std::sync::Arc::new(builder.finish()));
        <$crate::ast::$root as $crate::ast::AstNode>::cast(root).unwrap()
    }};
}
pub(crate) use quote_ as quote;

use crate::AstNode;

pub(crate) trait ToNodeChild {
    fn append_node_child(self, builder: &mut FlatTreeBuilder);
}

impl<N: AstNode> ToNodeChild for N {
    fn append_node_child(self, builder: &mut FlatTreeBuilder) {
        builder.node(self.syntax());
    }
}

impl<C: ToNodeChild> ToNodeChild for Option<C> {
    fn append_node_child(self, builder: &mut FlatTreeBuilder) {
        if let Some(child) = self {
            child.append_node_child(builder);
        }
    }
}

// This is useful when you want conditionally, based on some `bool`, to emit some code.
impl ToNodeChild for () {
    fn append_node_child(self, _builder: &mut FlatTreeBuilder) {}
}

pub(crate) const fn verify_only_whitespaces(text: &str) {
    let text = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if !text[i].is_ascii_whitespace() {
            panic!("non-whitespace found in whitespace token");
        }
        i += 1;
    }
}
