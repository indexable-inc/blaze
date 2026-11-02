//! Lowering of inline assembly.
use blaze_hash::{HashMap, HashSet};
use hir_expand::name::Name;
use intern::Symbol;
use syntax::{
    T,
    view::{self as ast, AstNode, AstToken, HasName, IsString},
};
use tt::TextRange;

use crate::{
    expr_store::{
        ExprPtr,
        lower::{ExprCollector, FxIndexSet, ImplTraitLowering},
    },
    hir::{
        AsmOperand, AsmOptions, AsmPiece, ExprId, InlineAsm, InlineAsmKind, InlineAsmRegOrRegClass,
    },
};

impl ExprCollector<'_> {
    pub(super) fn lower_inline_asm(
        &mut self,
        asm: ast::AsmExpr<'_>,
        syntax_ptr: ExprPtr,
    ) -> ExprId {
        let mut clobber_abis = FxIndexSet::default();
        // alloc: the operand list, once per `asm!` call; it becomes the boxed slice it stores.
        let mut operands = vec![];
        let mut options = AsmOptions::empty();

        let mut named_pos: HashMap<usize, Symbol> = Default::default();
        let mut named_args: HashMap<Symbol, usize> = Default::default();
        let mut reg_args: HashSet<usize> = Default::default();
        for piece in asm.asm_pieces() {
            if !self.check_cfg(piece) {
                continue;
            }

            let slot = operands.len();
            let mut lower_reg = |reg: Option<ast::AsmRegSpec<'_>>| {
                let reg = reg?;
                if let Some(string) = reg.string_token() {
                    reg_args.insert(slot);
                    Some(InlineAsmRegOrRegClass::Reg(Symbol::intern(string.text())))
                } else {
                    reg.name_ref().map(|name_ref| {
                        InlineAsmRegOrRegClass::RegClass(Symbol::intern(name_ref.text()))
                    })
                }
            };

            let op = match piece {
                ast::AsmPiece::AsmClobberAbi(clobber_abi) => {
                    if let Some(abi_name) = clobber_abi.string_token() {
                        clobber_abis.insert(Symbol::intern(abi_name.text()));
                    }
                    continue;
                }
                ast::AsmPiece::AsmOptions(opt) => {
                    opt.asm_options().for_each(|opt| {
                        options |= match opt.syntax().first_token().map_or(T![$], |it| it.kind()) {
                            T![att_syntax] => AsmOptions::ATT_SYNTAX,
                            T![may_unwind] => AsmOptions::MAY_UNWIND,
                            T![nomem] => AsmOptions::NOMEM,
                            T![noreturn] => AsmOptions::NORETURN,
                            T![nostack] => AsmOptions::NOSTACK,
                            T![preserves_flags] => AsmOptions::PRESERVES_FLAGS,
                            T![pure] => AsmOptions::PURE,
                            T![raw] => AsmOptions::RAW,
                            T![readonly] => AsmOptions::READONLY,
                            _ => return,
                        }
                    });
                    continue;
                }
                ast::AsmPiece::AsmOperandNamed(op) => {
                    let name = op.name().map(|name| Symbol::intern(name.text()));
                    if let Some(name) = name {
                        named_args.insert(name, slot);
                        named_pos.insert(slot, name);
                    }
                    let Some(op) = op.asm_operand() else { continue };
                    (
                        name.map(Name::new_symbol_root),
                        match op {
                            ast::AsmOperand::AsmRegOperand(op) => {
                                let Some(dir_spec) = op.asm_dir_spec() else {
                                    continue;
                                };
                                let Some(reg) = lower_reg(op.asm_reg_spec()) else {
                                    continue;
                                };
                                if dir_spec.in_token().is_some() {
                                    let expr = self.collect_expr_opt(
                                        op.asm_operand_expr().and_then(|it| it.in_expr()),
                                    );
                                    AsmOperand::In { reg, expr }
                                } else if dir_spec.out_token().is_some() {
                                    let expr = op
                                        .asm_operand_expr()
                                        .and_then(|it| it.in_expr())
                                        .filter(|it| !matches!(it, ast::Expr::UnderscoreExpr(_)))
                                        .map(|expr| self.collect_expr(expr));
                                    AsmOperand::Out { reg, expr, late: false }
                                } else if dir_spec.lateout_token().is_some() {
                                    let expr = op
                                        .asm_operand_expr()
                                        .and_then(|it| it.in_expr())
                                        .filter(|it| !matches!(it, ast::Expr::UnderscoreExpr(_)))
                                        .map(|expr| self.collect_expr(expr));

                                    AsmOperand::Out { reg, expr, late: true }
                                } else if dir_spec.inout_token().is_some() {
                                    let Some(op_expr) = op.asm_operand_expr() else { continue };
                                    let in_expr = self.collect_expr_opt(op_expr.in_expr());
                                    match op_expr.fat_arrow_token().is_some() {
                                        true => {
                                            // `out_expr` of the generated AST is the first
                                            // expression, which is the one going in.
                                            let out_expr = op_expr
                                                .syntax()
                                                .children()
                                                .filter_map(ast::Expr::cast)
                                                .nth(1)
                                                .filter(|it| {
                                                    !matches!(it, ast::Expr::UnderscoreExpr(_))
                                                })
                                                .map(|expr| self.collect_expr(expr));

                                            AsmOperand::SplitInOut {
                                                reg,
                                                in_expr,
                                                out_expr,
                                                late: false,
                                            }
                                        }
                                        false => {
                                            AsmOperand::InOut { reg, expr: in_expr, late: false }
                                        }
                                    }
                                } else if dir_spec.inlateout_token().is_some() {
                                    let Some(op_expr) = op.asm_operand_expr() else { continue };
                                    let in_expr = self.collect_expr_opt(op_expr.in_expr());
                                    match op_expr.fat_arrow_token().is_some() {
                                        true => {
                                            // `out_expr` of the generated AST is the first
                                            // expression, which is the one going in.
                                            let out_expr = op_expr
                                                .syntax()
                                                .children()
                                                .filter_map(ast::Expr::cast)
                                                .nth(1)
                                                .filter(|it| {
                                                    !matches!(it, ast::Expr::UnderscoreExpr(_))
                                                })
                                                .map(|expr| self.collect_expr(expr));

                                            AsmOperand::SplitInOut {
                                                reg,
                                                in_expr,
                                                out_expr,
                                                late: true,
                                            }
                                        }
                                        false => {
                                            AsmOperand::InOut { reg, expr: in_expr, late: true }
                                        }
                                    }
                                } else {
                                    continue;
                                }
                            }
                            ast::AsmOperand::AsmLabel(l) => {
                                AsmOperand::Label(self.collect_block_opt(l.block_expr()))
                            }
                            ast::AsmOperand::AsmConst(c) => {
                                AsmOperand::Const(self.collect_expr_opt(c.expr()))
                            }
                            ast::AsmOperand::AsmSym(s) => {
                                let Some(path) = s.path().and_then(|p| {
                                    self.lower_path_view(
                                        p,
                                        &mut ImplTraitLowering::Error,
                                    )
                                }) else {
                                    continue;
                                };
                                AsmOperand::Sym(path)
                            }
                        },
                    )
                }
            };
            operands.push(op);
        }

        let record = self.store.record;
        // alloc: the source-map side table, once per `asm!` call.
        let mut mappings = vec![];
        let mut curarg = 0;
        // alloc: the template it stores, once per `asm!` call.
        let mut template = Some(Vec::new());
        if options.contains(AsmOptions::RAW) {
            for it in asm.template() {
                if !self.check_cfg(it) {
                    continue;
                }
                let text = self
                    .expand_macros_to_string(it)
                    .and_then(|(s, _)| s.value().ok().map(|it| it.into_owned()));
                match (&mut template, text) {
                    (Some(pieces), Some(text)) => {
                        // alloc: the stored literal piece, once per raw template string.
                        pieces.push(AsmPiece::Lit(format!("{text}\n").into()))
                    }
                    _ => template = None,
                }
            }
        }
        let mut comment_only = !options.contains(AsmOptions::RAW);
        if !options.contains(AsmOptions::RAW) {
            // Don't treat raw asm as a format string.
            asm.template()
                .enumerate()
                .filter_map(|(idx, it)| {
                    if !self.check_cfg(it) {
                        return None;
                    }
                    Some((idx, it, self.expand_macros_to_string(it)?))
                })
                .for_each(|(idx, expr, (s, is_direct_literal))| {
                    if record {
                        mappings.resize_with(idx + 1, Vec::default);
                    }
                    let Ok(text) = s.value() else {
                        comment_only = false;
                        template = None;
                        return;
                    };
                    let line = text.trim();
                    comment_only &= line.is_empty()
                        || line
                            .strip_prefix("/*")
                            .and_then(|it| it.strip_suffix("*/"))
                            .is_some_and(|it| !it.contains("*/"));
                    let mut mappings = mappings.get_mut(idx);
                    let template_snippet = match expr {
                        ast::Expr::Literal(literal) => match literal.kind() {
                            // alloc: the parser takes an owned snippet, once per template string.
                            ast::LiteralKind::String(s) => Some(s.text().to_owned()),
                            _ => None,
                        },
                        _ => None,
                    };
                    let str_style = match s.quote_offsets() {
                        Some(offsets) => {
                            let raw = usize::from(offsets.quotes.0.len()) - 1;
                            // subtract 1 for the `r` prefix
                            (raw != 0).then(|| raw - 1)
                        }
                        None => None,
                    };

                    let mut parser = rustc_parse_format::Parser::new(
                        &text,
                        str_style,
                        template_snippet,
                        false,
                        rustc_parse_format::ParseMode::InlineAsm,
                    );
                    parser.curarg = curarg;

                    // alloc: the parser's pieces, once per template string.
                    let mut unverified_pieces = Vec::new();
                    while let Some(piece) = parser.next() {
                        if !parser.errors.is_empty() {
                            break;
                        } else {
                            unverified_pieces.push(piece);
                        }
                    }
                    if !parser.errors.is_empty() {
                        template = None;
                    }

                    curarg = parser.curarg;

                    let to_span = |inner_span: std::ops::Range<usize>| {
                        is_direct_literal.then(|| {
                            TextRange::new(
                                inner_span.start.try_into().unwrap(),
                                inner_span.end.try_into().unwrap(),
                            )
                        })
                    };
                    for piece in unverified_pieces {
                        match piece {
                            rustc_parse_format::Piece::Lit(lit) => {
                                if let Some(pieces) = &mut template {
                                    pieces.push(AsmPiece::Lit(lit.into()));
                                }
                            }
                            rustc_parse_format::Piece::NextArgument(arg) => {
                                // let span = arg_spans.next();

                                let (operand_idx, _name) = match arg.position {
                                    rustc_parse_format::ArgumentIs(idx)
                                    | rustc_parse_format::ArgumentImplicitlyIs(idx) => {
                                        if idx >= operands.len()
                                            || named_pos.contains_key(&idx)
                                            || reg_args.contains(&idx)
                                        {
                                            (None, None)
                                        } else {
                                            (Some(idx), None)
                                        }
                                    }
                                    rustc_parse_format::ArgumentNamed(name) => {
                                        let name = Symbol::intern(name);
                                        (
                                            named_args.get(&name).copied(),
                                            Some(Name::new_symbol_root(name)),
                                        )
                                    }
                                };

                                match (&mut template, operand_idx) {
                                    (Some(pieces), Some(index)) => pieces.push(AsmPiece::Operand {
                                        index,
                                        modifier: arg.format.ty.chars().next(),
                                    }),
                                    _ => template = None,
                                }
                                if let Some(mappings) = &mut mappings
                                    && let Some(operand_idx) = operand_idx
                                    && let Some(position_span) = to_span(arg.position_span)
                                {
                                    mappings.push((position_span, operand_idx));
                                }
                            }
                        }
                    }
                    if let Some(pieces) = &mut template {
                        pieces.push(AsmPiece::Lit("\n".into()));
                    }
                })
        };

        let kind = if asm.global_asm_token().is_some() {
            InlineAsmKind::GlobalAsm
        } else if asm.naked_asm_token().is_some() {
            InlineAsmKind::NakedAsm
        } else {
            InlineAsmKind::Asm
        };

        let idx = self.store.alloc_asm_expr(InlineAsm {
            operands: operands.into_boxed_slice(),
            template: template.map(Vec::into_boxed_slice),
            options,
            kind,
            comment_only,
            clobber_abi: !clobber_abis.is_empty(),
        });
        let idx = self.map_expr(idx, syntax_ptr);
        if record {
            self.store
                .template_map
                .get_or_insert_with(Default::default)
                .asm_to_captures
                .push((idx, mappings));
        }
        idx
    }
}
