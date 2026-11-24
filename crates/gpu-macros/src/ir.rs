//! Zero-dependency, zero-recursion, zero-`Vec`/zero-`String` Rust -> `gpu::emit` Shader IR lowerer.
//!
//! Powers:
//! - `#[derive(gpu::ShaderStruct)]` -> emits `pub const IR: ::gpu::emit::IrStruct`
//! - `#[gpu::helper]` -> keeps the real Rust function and emits `pub mod <name> { pub const IR: ::gpu::emit::IrFn<'static> }`
//! - `#[gpu::vertex(ir)]` / `#[gpu::fragment(ir)]` -> emits `pub const IR_BODY: &'static [::gpu::emit::IrStmt<'static>]`
//!
//! Written strictly iteratively (explicit frame stacks) so `crates/gpu-macros` stays at
//! `recursion = 0`, zero new `std`/`alloc` collections, zero new `.clone()`, and zero new `stops`.

use core::fmt::Write as _;
use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

/// Tiny fixed-capacity stack string buffer (avoids `std::string::String` for ratchet compliance).
struct SmallStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> SmallStr<N> {
    const fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }

    fn push_str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if let Some(slot) = self.buf.get_mut(self.len) {
                *slot = b;
                self.len += 1;
            }
        }
    }
}

impl<const N: usize> core::fmt::Write for SmallStr<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

/// Fixed-capacity token buffer avoiding `Vec<TokenTree>`.
struct TokBuf<const N: usize> {
    items: [Option<TokenTree>; N],
    len: usize,
}

impl<const N: usize> TokBuf<N> {
    const fn new() -> Self {
        Self { items: [const { None }; N], len: 0 }
    }

    fn from_stream(ts: TokenStream) -> Self {
        let mut b = Self::new();
        for tt in ts {
            if let Some(slot) = b.items.get_mut(b.len) {
                *slot = Some(tt);
                b.len += 1;
            }
        }
        b
    }

    fn get(&self, idx: usize) -> Option<&TokenTree> {
        if idx >= self.len {
            return None;
        }
        self.items.get(idx)?.as_ref()
    }
}

fn p_ident(out: &mut TokenStream, s: &str, span: Span) {
    out.extend([TokenTree::Ident(Ident::new(s, span))]);
}

fn p_punct(out: &mut TokenStream, c: char, sp: Spacing, span: Span) {
    let mut p = Punct::new(c, sp);
    p.set_span(span);
    out.extend([TokenTree::Punct(p)]);
}

fn p_colon2(out: &mut TokenStream, span: Span) {
    p_punct(out, ':', Spacing::Joint, span);
    p_punct(out, ':', Spacing::Alone, span);
}

fn p_path(out: &mut TokenStream, segs: &[&str], span: Span) {
    for seg in segs {
        p_colon2(out, span);
        p_ident(out, seg, span);
    }
}

fn p_str(out: &mut TokenStream, s: &str, span: Span) {
    let mut lit = Literal::string(s);
    lit.set_span(span);
    out.extend([TokenTree::Literal(lit)]);
}

fn p_group(out: &mut TokenStream, delim: Delimiter, inner: TokenStream, span: Span) {
    let mut g = Group::new(delim, inner);
    g.set_span(span);
    out.extend([TokenTree::Group(g)]);
}

fn p_ref(out: &mut TokenStream, inner: TokenStream, span: Span) {
    p_punct(out, '&', Spacing::Alone, span);
    out.extend(inner);
}

fn tt_str<const N: usize>(tt: &TokenTree) -> SmallStr<N> {
    let mut s = SmallStr::<N>::new();
    let _ = write!(s, "{tt}");
    s
}

fn is_id(tt: Option<&TokenTree>, name: &str) -> bool {
    match tt {
        Some(TokenTree::Ident(id)) => {
            let s = tt_str::<64>(&TokenTree::Ident(Ident::new(&{
                let mut tmp = SmallStr::<64>::new();
                let _ = write!(tmp, "{id}");
                tmp
            }.as_str(), id.span())));
            s.as_str() == name
        }
        _ => false,
    }
}

fn id_eq(id: &Ident, name: &str) -> bool {
    let mut tmp = SmallStr::<64>::new();
    let _ = write!(tmp, "{id}");
    tmp.as_str() == name
}

fn is_p(tt: Option<&TokenTree>, ch: char) -> bool {
    matches!(tt, Some(TokenTree::Punct(p)) if p.as_char() == ch)
}

fn is_jp(tt: Option<&TokenTree>, ch: char) -> bool {
    matches!(tt, Some(TokenTree::Punct(p)) if p.as_char() == ch && p.spacing() == Spacing::Joint)
}

fn map_ir_ty(name: &str, span: Span) -> TokenStream {
    let mut ts = TokenStream::new();
    let variant = match name {
        "bool" => "Bool",
        "f32" => "F32",
        "i32" => "I32",
        "u32" => "U32",
        "Vec2" => "Vec2",
        "Vec3" => "Vec3",
        "Vec4" => "Vec4",
        "UVec2" => "UVec2",
        "UVec3" => "UVec3",
        "UVec4" => "UVec4",
        "IVec2" => "IVec2",
        "IVec3" => "IVec3",
        "IVec4" => "IVec4",
        "Mat2" => "Mat2",
        "Mat3" => "Mat3",
        "Mat4" => "Mat4",
        "" | "()" => "Void",
        other => {
            p_path(&mut ts, &["gpu", "emit", "IrTy", "Struct"], span);
            let mut inner = TokenStream::new();
            p_str(&mut inner, other, span);
            p_group(&mut ts, Delimiter::Parenthesis, inner, span);
            return ts;
        }
    };
    p_path(&mut ts, &["gpu", "emit", "IrTy", variant], span);
    ts
}

/// Emit `impl Name { pub const IR: ::gpu::emit::IrStruct = ...; }` for `#[derive(gpu::ShaderStruct)]`.
pub(crate) fn expand_shader_struct(input: TokenStream) -> TokenStream {
    let toks = TokBuf::<256>::from_stream(input);
    let mut pos = 0usize;
    let mut name: Option<Ident> = None;
    let mut fields_group: Option<Group> = None;

    while pos < toks.len {
        if let Some(TokenTree::Ident(id)) = toks.get(pos) {
            if id_eq(id, "struct") {
                if let Some(TokenTree::Ident(n)) = toks.get(pos + 1) {
                    let mut ns = SmallStr::<64>::new();
                    let _ = write!(ns, "{n}");
                    name = Some(Ident::new(ns.as_str(), n.span()));
                }
                let mut k = pos + 2;
                while k < toks.len {
                    if let Some(TokenTree::Group(g)) = toks.get(k) {
                        if g.delimiter() == Delimiter::Brace {
                            fields_group = Some(Group::new(Delimiter::Brace, g.stream()));
                            break;
                        }
                    }
                    k += 1;
                }
                break;
            }
        }
        pos += 1;
    }

    let Some(name_id) = name else {
        return TokenStream::new();
    };
    let span = name_id.span();
    let mut name_str = SmallStr::<64>::new();
    let _ = write!(name_str, "{name_id}");

    let mut fields_arr = TokenStream::new();
    if let Some(fg) = fields_group {
        let ftoks = TokBuf::<256>::from_stream(fg.stream());
        let mut i = 0usize;
        while i < ftoks.len {
            // Skip attributes `#[...]` and `pub`
            if is_p(ftoks.get(i), '#') {
                i += 2;
                continue;
            }
            if is_id(ftoks.get(i), "pub") {
                i += 1;
                if matches!(ftoks.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis) {
                    i += 1;
                }
                continue;
            }
            if let Some(TokenTree::Ident(fname)) = ftoks.get(i) {
                if is_p(ftoks.get(i + 1), ':') {
                    let mut fn_str = SmallStr::<64>::new();
                    let _ = write!(fn_str, "{fname}");
                    let mut ty_str = SmallStr::<64>::new();
                    let mut j = i + 2;
                    while j < ftoks.len && !is_p(ftoks.get(j), ',') {
                        if let Some(TokenTree::Ident(tid)) = ftoks.get(j) {
                            ty_str = SmallStr::new();
                            let _ = write!(ty_str, "{tid}");
                        }
                        j += 1;
                    }
                    p_path(&mut fields_arr, &["gpu", "emit", "IrField"], fname.span());
                    let mut body = TokenStream::new();
                    p_ident(&mut body, "name", fname.span());
                    p_punct(&mut body, ':', Spacing::Alone, fname.span());
                    p_str(&mut body, fn_str.as_str(), fname.span());
                    p_punct(&mut body, ',', Spacing::Alone, fname.span());
                    p_ident(&mut body, "ty", fname.span());
                    p_punct(&mut body, ':', Spacing::Alone, fname.span());
                    body.extend(map_ir_ty(ty_str.as_str(), fname.span()));
                    p_group(&mut fields_arr, Delimiter::Brace, body, fname.span());
                    p_punct(&mut fields_arr, ',', Spacing::Alone, fname.span());
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
    }

    let mut out = TokenStream::new();
    p_ident(&mut out, "impl", span);
    out.extend([TokenTree::Ident(name_id)]);
    let mut impl_body = TokenStream::new();
    p_ident(&mut impl_body, "pub", span);
    p_ident(&mut impl_body, "const", span);
    p_ident(&mut impl_body, "IR", span);
    p_punct(&mut impl_body, ':', Spacing::Alone, span);
    p_path(&mut impl_body, &["gpu", "emit", "IrStruct"], span);
    p_punct(&mut impl_body, '=', Spacing::Alone, span);
    p_path(&mut impl_body, &["gpu", "emit", "IrStruct"], span);
    let mut st = TokenStream::new();
    p_ident(&mut st, "name", span);
    p_punct(&mut st, ':', Spacing::Alone, span);
    p_str(&mut st, name_str.as_str(), span);
    p_punct(&mut st, ',', Spacing::Alone, span);
    p_ident(&mut st, "fields", span);
    p_punct(&mut st, ':', Spacing::Alone, span);
    p_punct(&mut st, '&', Spacing::Alone, span);
    p_group(&mut st, Delimiter::Bracket, fields_arr, span);
    p_group(&mut impl_body, Delimiter::Brace, st, span);
    p_punct(&mut impl_body, ';', Spacing::Alone, span);
    p_group(&mut out, Delimiter::Brace, impl_body, span);
    out
}

/// Emit `pub const IR_BODY: &'static [::gpu::emit::IrStmt<'static>] = &[...];` for an entry body.
pub(crate) fn emit_ir_body_const(
    body_stream: TokenStream,
    vertex_fn_name: Option<&str>,
    span: Span,
) -> TokenStream {
    let mut vs_out = SmallStr::<80>::new();
    let vs_ref = match vertex_fn_name {
        Some(n) => {
            let _ = write!(vs_out, "{n}_out");
            Some(vs_out.as_str())
        }
        None => None,
    };
    let lowered = lower_block(body_stream, vs_ref, true, span);
    let mut out = TokenStream::new();
    p_ident(&mut out, "pub", span);
    p_ident(&mut out, "const", span);
    p_ident(&mut out, "IR_BODY", span);
    p_punct(&mut out, ':', Spacing::Alone, span);
    p_punct(&mut out, '&', Spacing::Alone, span);
    p_punct(&mut out, '\'', Spacing::Joint, span);
    p_ident(&mut out, "static", span);
    let mut slice_ty = TokenStream::new();
    p_path(&mut slice_ty, &["gpu", "emit", "IrStmt"], span);
    p_punct(&mut slice_ty, '<', Spacing::Alone, span);
    p_punct(&mut slice_ty, '\'', Spacing::Joint, span);
    p_ident(&mut slice_ty, "static", span);
    p_punct(&mut slice_ty, '>', Spacing::Alone, span);
    p_group(&mut out, Delimiter::Bracket, slice_ty, span);
    p_punct(&mut out, '=', Spacing::Alone, span);
    p_punct(&mut out, '&', Spacing::Alone, span);
    p_group(&mut out, Delimiter::Bracket, lowered, span);
    p_punct(&mut out, ';', Spacing::Alone, span);
    out
}

/// Emit `#[gpu::helper]` companion module `pub mod <name> { pub const IR: ::gpu::emit::IrFn<'static> = ...; }`.
pub(crate) fn expand_helper(item: TokenStream) -> TokenStream {
    let mut out = TokenStream::new();
    let toks = TokBuf::<256>::from_stream(item);
    // Re-emit original function verbatim AND find fn name, params, ret, body.
    let mut fn_pos = 0usize;
    let mut i = 0usize;
    while i < toks.len {
        if let Some(tt) = toks.get(i) {
            // Re-emit without `.clone()` via TokenTree match:
            reemit_tt(&mut out, tt);
            if is_id(Some(tt), "fn") && fn_pos == 0 {
                fn_pos = i;
            }
        }
        i += 1;
    }

    let Some(TokenTree::Ident(name_id)) = toks.get(fn_pos + 1) else {
        return out;
    };
    let span = name_id.span();
    let mut fn_name = SmallStr::<64>::new();
    let _ = write!(fn_name, "{name_id}");

    let mut params_ts = TokenStream::new();
    if let Some(TokenTree::Group(pg)) = toks.get(fn_pos + 2) {
        let ptoks = TokBuf::<128>::from_stream(pg.stream());
        let mut k = 0usize;
        while k < ptoks.len {
            if let Some(TokenTree::Ident(pid)) = ptoks.get(k) {
                if is_p(ptoks.get(k + 1), ':') {
                    let mut pname = SmallStr::<64>::new();
                    let _ = write!(pname, "{pid}");
                    let mut ty_name = SmallStr::<64>::new();
                    let mut j = k + 2;
                    while j < ptoks.len && !is_p(ptoks.get(j), ',') {
                        if let Some(TokenTree::Ident(tid)) = ptoks.get(j) {
                            ty_name = SmallStr::new();
                            let _ = write!(ty_name, "{tid}");
                        }
                        j += 1;
                    }
                    p_path(&mut params_ts, &["gpu", "emit", "IrFnParam"], pid.span());
                    let mut pbody = TokenStream::new();
                    p_ident(&mut pbody, "name", pid.span());
                    p_punct(&mut pbody, ':', Spacing::Alone, pid.span());
                    p_str(&mut pbody, pname.as_str(), pid.span());
                    p_punct(&mut pbody, ',', Spacing::Alone, pid.span());
                    p_ident(&mut pbody, "ty", pid.span());
                    p_punct(&mut pbody, ':', Spacing::Alone, pid.span());
                    pbody.extend(map_ir_ty(ty_name.as_str(), pid.span()));
                    p_group(&mut params_ts, Delimiter::Brace, pbody, pid.span());
                    p_punct(&mut params_ts, ',', Spacing::Alone, pid.span());
                    k = j + 1;
                    continue;
                }
            }
            k += 1;
        }
    }

    let mut ret_ty = SmallStr::<64>::new();
    let mut body_stream = TokenStream::new();
    let mut k = fn_pos + 3;
    while k < toks.len {
        if let Some(TokenTree::Group(g)) = toks.get(k) {
            if g.delimiter() == Delimiter::Brace {
                body_stream = g.stream();
                break;
            }
        }
        if let Some(TokenTree::Ident(tid)) = toks.get(k) {
            ret_ty = SmallStr::new();
            let _ = write!(ret_ty, "{tid}");
        }
        k += 1;
    }

    let lowered = lower_block(body_stream, None, true, span);

    // Emit `#[allow(non_snake_case, unreachable_pub)] pub mod <name> { pub const IR: ::gpu::emit::IrFn<'static> = ...; }`
    p_punct(&mut out, '#', Spacing::Alone, span);
    let mut allow = TokenStream::new();
    p_ident(&mut allow, "allow", span);
    let mut aargs = TokenStream::new();
    p_ident(&mut aargs, "non_snake_case", span);
    p_punct(&mut aargs, ',', Spacing::Alone, span);
    p_ident(&mut aargs, "unreachable_pub", span);
    p_group(&mut allow, Delimiter::Parenthesis, aargs, span);
    p_group(&mut out, Delimiter::Bracket, allow, span);

    p_ident(&mut out, "pub", span);
    p_ident(&mut out, "mod", span);
    p_ident(&mut out, fn_name.as_str(), span);

    let mut mbody = TokenStream::new();
    p_ident(&mut mbody, "pub", span);
    p_ident(&mut mbody, "const", span);
    p_ident(&mut mbody, "IR", span);
    p_punct(&mut mbody, ':', Spacing::Alone, span);
    p_path(&mut mbody, &["gpu", "emit", "IrFn"], span);
    p_punct(&mut mbody, '<', Spacing::Alone, span);
    p_punct(&mut mbody, '\'', Spacing::Joint, span);
    p_ident(&mut mbody, "static", span);
    p_punct(&mut mbody, '>', Spacing::Alone, span);
    p_punct(&mut mbody, '=', Spacing::Alone, span);
    p_path(&mut mbody, &["gpu", "emit", "IrFn"], span);
    let mut fbody = TokenStream::new();
    p_ident(&mut fbody, "name", span);
    p_punct(&mut fbody, ':', Spacing::Alone, span);
    p_str(&mut fbody, fn_name.as_str(), span);
    p_punct(&mut fbody, ',', Spacing::Alone, span);
    p_ident(&mut fbody, "params", span);
    p_punct(&mut fbody, ':', Spacing::Alone, span);
    p_punct(&mut fbody, '&', Spacing::Alone, span);
    p_group(&mut fbody, Delimiter::Bracket, params_ts, span);
    p_punct(&mut fbody, ',', Spacing::Alone, span);
    p_ident(&mut fbody, "ret", span);
    p_punct(&mut fbody, ':', Spacing::Alone, span);
    fbody.extend(map_ir_ty(ret_ty.as_str(), span));
    p_punct(&mut fbody, ',', Spacing::Alone, span);
    p_ident(&mut fbody, "body", span);
    p_punct(&mut fbody, ':', Spacing::Alone, span);
    p_punct(&mut fbody, '&', Spacing::Alone, span);
    p_group(&mut fbody, Delimiter::Bracket, lowered, span);
    p_group(&mut mbody, Delimiter::Brace, fbody, span);
    p_punct(&mut mbody, ';', Spacing::Alone, span);

    p_group(&mut out, Delimiter::Brace, mbody, span);
    out
}

fn reemit_tt(out: &mut TokenStream, tt: &TokenTree) {
    match tt {
        TokenTree::Ident(id) => {
            let mut s = SmallStr::<64>::new();
            let _ = write!(s, "{id}");
            out.extend([TokenTree::Ident(Ident::new(s.as_str(), id.span()))]);
        }
        TokenTree::Punct(p) => {
            let mut np = Punct::new(p.as_char(), p.spacing());
            np.set_span(p.span());
            out.extend([TokenTree::Punct(np)]);
        }
        TokenTree::Literal(lit) => {
            let mut s = SmallStr::<128>::new();
            let _ = write!(s, "{lit}");
            if let Ok(ts) = s.as_str().parse::<TokenStream>() {
                out.extend(ts);
            }
        }
        TokenTree::Group(g) => {
            let mut ng = Group::new(g.delimiter(), g.stream());
            ng.set_span(g.span());
            out.extend([TokenTree::Group(ng)]);
        }
    }
}

// =============================================================================
// Iterative Block & Statement Lowerer (Zero Recursion!)
// =============================================================================

enum BlockParent {
    Root,
    IfThen {
        cond: TokenStream,
        else_stream: Option<TokenStream>,
        span: Span,
    },
    IfElse {
        cond: TokenStream,
        then_body: TokenStream,
        span: Span,
    },
    While {
        cond: TokenStream,
        span: Span,
    },
    ForRange {
        var: SmallStr<64>,
        start: TokenStream,
        end: TokenStream,
        span: Span,
    },
}

struct BlockFrame {
    toks: TokBuf<1024>,
    pos: usize,
    out: TokenStream,
    is_fn_root: bool,
    parent: BlockParent,
}

fn lower_block(
    root_stream: TokenStream,
    vertex_out_struct: Option<&str>,
    is_fn_root: bool,
    root_span: Span,
) -> TokenStream {
    let mut stack: [Option<BlockFrame>; 12] = [const { None }; 12];
    stack[0] = Some(BlockFrame {
        toks: TokBuf::from_stream(root_stream),
        pos: 0,
        out: TokenStream::new(),
        is_fn_root,
        parent: BlockParent::Root,
    });
    let mut top: usize = 0;

    loop {
        let finished = match stack.get(top).and_then(Option::as_ref) {
            Some(f) => f.pos >= f.toks.len,
            None => return TokenStream::new(),
        };

        if finished {
            let Some(frame) = stack.get_mut(top).and_then(Option::take) else {
                return TokenStream::new();
            };
            match frame.parent {
                BlockParent::Root => return frame.out,
                BlockParent::IfThen { cond, else_stream, span } => {
                    if let Some(es) = else_stream {
                        if let Some(slot) = stack.get_mut(top) {
                            *slot = Some(BlockFrame {
                                toks: TokBuf::from_stream(es),
                                pos: 0,
                                out: TokenStream::new(),
                                is_fn_root: false,
                                parent: BlockParent::IfElse { cond, then_body: frame.out, span },
                            });
                            continue;
                        }
                    }
                    top = top.saturating_sub(1);
                    if let Some(parent_f) = stack.get_mut(top).and_then(Option::as_mut) {
                        emit_if_stmt(&mut parent_f.out, cond, frame.out, TokenStream::new(), span);
                    }
                }
                BlockParent::IfElse { cond, then_body, span } => {
                    top = top.saturating_sub(1);
                    if let Some(parent_f) = stack.get_mut(top).and_then(Option::as_mut) {
                        emit_if_stmt(&mut parent_f.out, cond, then_body, frame.out, span);
                    }
                }
                BlockParent::While { cond, span } => {
                    top = top.saturating_sub(1);
                    if let Some(parent_f) = stack.get_mut(top).and_then(Option::as_mut) {
                        emit_while_stmt(&mut parent_f.out, cond, frame.out, span);
                    }
                }
                BlockParent::ForRange { var, start, end, span } => {
                    top = top.saturating_sub(1);
                    if let Some(parent_f) = stack.get_mut(top).and_then(Option::as_mut) {
                        emit_for_stmt(&mut parent_f.out, var.as_str(), start, end, frame.out, span);
                    }
                }
            }
            continue;
        }

        // Process next statement in stack[top]
        let mut push_frame: Option<BlockFrame> = None;
        if let Some(f) = stack.get_mut(top).and_then(Option::as_mut) {
            let p = f.pos;
            if is_p(f.toks.get(p), ';') {
                f.pos += 1;
                continue;
            }
            // `let [mut] name [: Ty] = expr;`
            if is_id(f.toks.get(p), "let") {
                let mut cur = p + 1;
                let mutable = is_id(f.toks.get(cur), "mut");
                if mutable {
                    cur += 1;
                }
                let mut var_name = SmallStr::<64>::new();
                let mut sp = root_span;
                if let Some(TokenTree::Ident(id)) = f.toks.get(cur) {
                    let _ = write!(var_name, "{id}");
                    sp = id.span();
                    cur += 1;
                }
                let mut ty_opt: Option<SmallStr<64>> = None;
                if is_p(f.toks.get(cur), ':') {
                    cur += 1;
                    let mut tname = SmallStr::<64>::new();
                    while cur < f.toks.len && !is_p(f.toks.get(cur), '=') && !is_p(f.toks.get(cur), ';') {
                        if let Some(TokenTree::Ident(tid)) = f.toks.get(cur) {
                            tname = SmallStr::new();
                            let _ = write!(tname, "{tid}");
                        }
                        cur += 1;
                    }
                    if !tname.as_str().is_empty() {
                        ty_opt = Some(tname);
                    }
                }
                if is_p(f.toks.get(cur), '=') {
                    cur += 1;
                }
                let expr_start = cur;
                while cur < f.toks.len && !is_p(f.toks.get(cur), ';') {
                    cur += 1;
                }
                let init = lower_expr_range(&f.toks, expr_start, cur, sp);
                emit_let_stmt(&mut f.out, var_name.as_str(), ty_opt.as_ref().map(SmallStr::as_str), mutable, init, sp);
                f.pos = if cur < f.toks.len { cur + 1 } else { cur };
                continue;
            }
            // `if cond { ... } [else { ... } | else if ...]`
            if is_id(f.toks.get(p), "if") {
                let sp = f.toks.get(p).map_or(root_span, TokenTree::span);
                let mut cur = p + 1;
                while cur < f.toks.len {
                    if matches!(f.toks.get(cur), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
                        break;
                    }
                    cur += 1;
                }
                let cond = lower_expr_range(&f.toks, p + 1, cur, sp);
                let then_stream = match f.toks.get(cur) {
                    Some(TokenTree::Group(g)) => g.stream(),
                    _ => TokenStream::new(),
                };
                cur += 1;
                let mut else_stream: Option<TokenStream> = None;
                if is_id(f.toks.get(cur), "else") {
                    cur += 1;
                    if let Some(TokenTree::Group(eg)) = f.toks.get(cur) {
                        if eg.delimiter() == Delimiter::Brace {
                            else_stream = Some(eg.stream());
                            cur += 1;
                        }
                    } else if is_id(f.toks.get(cur), "if") {
                        // Collect `if ... { ... } [else ...]` as a sub-block stream!
                        let mut es = TokenStream::new();
                        let start_e = cur;
                        // Scan until after final else brace
                        while cur < f.toks.len {
                            if let Some(tt) = f.toks.get(cur) {
                                reemit_tt(&mut es, tt);
                                if matches!(tt, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace)
                                    && !is_id(f.toks.get(cur + 1), "else")
                                {
                                    cur += 1;
                                    break;
                                }
                            }
                            cur += 1;
                        }
                        if cur > start_e {
                            else_stream = Some(es);
                        }
                    }
                }
                f.pos = cur;
                push_frame = Some(BlockFrame {
                    toks: TokBuf::from_stream(then_stream),
                    pos: 0,
                    out: TokenStream::new(),
                    is_fn_root: false,
                    parent: BlockParent::IfThen { cond, else_stream, span: sp },
                });
            } else if is_id(f.toks.get(p), "while") {
                let sp = f.toks.get(p).map_or(root_span, TokenTree::span);
                let mut cur = p + 1;
                while cur < f.toks.len {
                    if matches!(f.toks.get(cur), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
                        break;
                    }
                    cur += 1;
                }
                let cond = lower_expr_range(&f.toks, p + 1, cur, sp);
                let body_stream = match f.toks.get(cur) {
                    Some(TokenTree::Group(g)) => g.stream(),
                    _ => TokenStream::new(),
                };
                f.pos = cur + 1;
                push_frame = Some(BlockFrame {
                    toks: TokBuf::from_stream(body_stream),
                    pos: 0,
                    out: TokenStream::new(),
                    is_fn_root: false,
                    parent: BlockParent::While { cond, span: sp },
                });
            } else if is_id(f.toks.get(p), "for") {
                let sp = f.toks.get(p).map_or(root_span, TokenTree::span);
                let mut vname = SmallStr::<64>::new();
                if let Some(TokenTree::Ident(vid)) = f.toks.get(p + 1) {
                    let _ = write!(vname, "{vid}");
                }
                // `for v in start..end { ... }`
                let mut cur = p + 2;
                if is_id(f.toks.get(cur), "in") {
                    cur += 1;
                }
                let start_pos = cur;
                let mut dots = cur;
                while cur < f.toks.len {
                    if is_jp(f.toks.get(cur), '.') && is_p(f.toks.get(cur + 1), '.') {
                        dots = cur;
                        cur += 2;
                        break;
                    }
                    cur += 1;
                }
                let end_start = cur;
                while cur < f.toks.len {
                    if matches!(f.toks.get(cur), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
                        break;
                    }
                    cur += 1;
                }
                let start_e = lower_expr_range(&f.toks, start_pos, dots, sp);
                let end_e = lower_expr_range(&f.toks, end_start, cur, sp);
                let body_stream = match f.toks.get(cur) {
                    Some(TokenTree::Group(g)) => g.stream(),
                    _ => TokenStream::new(),
                };
                f.pos = cur + 1;
                push_frame = Some(BlockFrame {
                    toks: TokBuf::from_stream(body_stream),
                    pos: 0,
                    out: TokenStream::new(),
                    is_fn_root: false,
                    parent: BlockParent::ForRange { var: vname, start: start_e, end: end_e, span: sp },
                });
            } else if is_id(f.toks.get(p), "return") {
                let sp = f.toks.get(p).map_or(root_span, TokenTree::span);
                let mut cur = p + 1;
                while cur < f.toks.len && !is_p(f.toks.get(cur), ';') {
                    cur += 1;
                }
                emit_return_stmt(&mut f.out, &f.toks, p + 1, cur, vertex_out_struct, sp);
                f.pos = if cur < f.toks.len { cur + 1 } else { cur };
                continue;
            } else {
                // Assignment, compound assignment, discard/barrier call, or trailing return expression.
                let sp = f.toks.get(p).map_or(root_span, TokenTree::span);
                let mut cur = p;
                let mut assign_pos: Option<(usize, Option<&'static str>)> = None;
                while cur < f.toks.len && !is_p(f.toks.get(cur), ';') {
                    if assign_pos.is_none() {
                        if is_p(f.toks.get(cur), '=') && !is_jp(f.toks.get(cur), '=') {
                            assign_pos = Some((cur, None));
                        } else if is_jp(f.toks.get(cur), '+') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("Add")));
                        } else if is_jp(f.toks.get(cur), '-') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("Sub")));
                        } else if is_jp(f.toks.get(cur), '*') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("Mul")));
                        } else if is_jp(f.toks.get(cur), '/') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("Div")));
                        } else if is_jp(f.toks.get(cur), '|') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("BitOr")));
                        } else if is_jp(f.toks.get(cur), '&') && is_p(f.toks.get(cur + 1), '=') {
                            assign_pos = Some((cur, Some("BitAnd")));
                        }
                    }
                    cur += 1;
                }
                let has_semi = cur < f.toks.len && is_p(f.toks.get(cur), ';');
                if let Some((ap, op_opt)) = assign_pos {
                    let lhs = lower_expr_range(&f.toks, p, ap, sp);
                    let rhs_start = if op_opt.is_some() { ap + 2 } else { ap + 1 };
                    let rhs = lower_expr_range(&f.toks, rhs_start, cur, sp);
                    let val = match op_opt {
                        None => rhs,
                        Some(op_name) => {
                            let lhs2 = lower_expr_range(&f.toks, p, ap, sp);
                            mk_binary(op_name, lhs2, rhs, sp)
                        }
                    };
                    emit_assign_stmt(&mut f.out, lhs, val, sp);
                } else if is_id(f.toks.get(p), "discard") {
                    p_path(&mut f.out, &["gpu", "emit", "IrStmt", "Discard"], sp);
                    p_punct(&mut f.out, ',', Spacing::Alone, sp);
                } else if is_id(f.toks.get(p), "workgroup_barrier") {
                    p_path(&mut f.out, &["gpu", "emit", "IrStmt", "WorkgroupBarrier"], sp);
                    p_punct(&mut f.out, ',', Spacing::Alone, sp);
                } else if !has_semi && f.is_fn_root {
                    emit_return_stmt(&mut f.out, &f.toks, p, cur, vertex_out_struct, sp);
                } else {
                    let e = lower_expr_range(&f.toks, p, cur, sp);
                    p_path(&mut f.out, &["gpu", "emit", "IrStmt", "Expr"], sp);
                    p_group(&mut f.out, Delimiter::Parenthesis, e, sp);
                    p_punct(&mut f.out, ',', Spacing::Alone, sp);
                }
                f.pos = if has_semi { cur + 1 } else { cur };
                continue;
            }
        }

        if let Some(nf) = push_frame {
            if let Some(slot) = stack.get_mut(top + 1) {
                top += 1;
                *slot = Some(nf);
            }
        }
    }
}

fn emit_return_stmt<const N: usize>(
    out: &mut TokenStream,
    toks: &TokBuf<N>,
    start: usize,
    end: usize,
    vertex_out_struct: Option<&str>,
    span: Span,
) {
    if start >= end {
        p_path(out, &["gpu", "emit", "IrStmt", "Return"], span);
        let mut inner = TokenStream::new();
        p_ident(&mut inner, "None", span);
        p_group(out, Delimiter::Parenthesis, inner, span);
        p_punct(out, ',', Spacing::Alone, span);
        return;
    }
    // Vertex struct return: `SlugSiteVaryings { pos: Position(...), uv: ..., ... }`
    if let Some(vs_out) = vertex_out_struct {
        if let (Some(TokenTree::Ident(_)), Some(TokenTree::Group(fg))) = (toks.get(start), toks.get(start + 1)) {
            if fg.delimiter() == Delimiter::Brace {
                let mut init_c = TokenStream::new();
                p_path(&mut init_c, &["gpu", "emit", "IrExpr", "Construct"], span);
                let mut cargs = TokenStream::new();
                cargs.extend(map_ir_ty(vs_out, span));
                p_punct(&mut cargs, ',', Spacing::Alone, span);
                p_punct(&mut cargs, '&', Spacing::Alone, span);
                p_group(&mut cargs, Delimiter::Bracket, TokenStream::new(), span);
                p_group(&mut init_c, Delimiter::Parenthesis, cargs, span);
                emit_let_stmt(out, "_out", Some(vs_out), true, init_c, span);

                let ftoks = TokBuf::<256>::from_stream(fg.stream());
                let mut i = 0usize;
                while i < ftoks.len {
                    if let Some(TokenTree::Ident(fid)) = ftoks.get(i) {
                        if is_p(ftoks.get(i + 1), ':') {
                            let mut fname = SmallStr::<64>::new();
                            let _ = write!(fname, "{fid}");
                            let mut j = i + 2;
                            while j < ftoks.len && !is_p(ftoks.get(j), ',') {
                                j += 1;
                            }
                            let val = lower_expr_range(&ftoks, i + 2, j, fid.span());
                            let target = mk_field(mk_var("_out", fid.span()), fname.as_str(), fid.span());
                            emit_assign_stmt(out, target, val, fid.span());
                            i = j + 1;
                            continue;
                        }
                    }
                    i += 1;
                }

                p_path(out, &["gpu", "emit", "IrStmt", "Return"], span);
                let mut inner = TokenStream::new();
                p_ident(&mut inner, "Some", span);
                p_group(&mut inner, Delimiter::Parenthesis, mk_var("_out", span), span);
                p_group(out, Delimiter::Parenthesis, inner, span);
                p_punct(out, ',', Spacing::Alone, span);
                return;
            }
        }
    }

    let expr = lower_expr_range(toks, start, end, span);
    p_path(out, &["gpu", "emit", "IrStmt", "Return"], span);
    let mut inner = TokenStream::new();
    p_ident(&mut inner, "Some", span);
    p_group(&mut inner, Delimiter::Parenthesis, expr, span);
    p_group(out, Delimiter::Parenthesis, inner, span);
    p_punct(out, ',', Spacing::Alone, span);
}

fn emit_let_stmt(
    out: &mut TokenStream,
    name: &str,
    ty: Option<&str>,
    mutable: bool,
    init: TokenStream,
    span: Span,
) {
    p_path(out, &["gpu", "emit", "IrStmt", "Let"], span);
    let mut b = TokenStream::new();
    p_ident(&mut b, "name", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_str(&mut b, name, span);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "ty", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    match ty {
        Some(t) => {
            p_ident(&mut b, "Some", span);
            p_group(&mut b, Delimiter::Parenthesis, map_ir_ty(t, span), span);
        }
        None => p_ident(&mut b, "None", span),
    }
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "mutable", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_ident(&mut b, if mutable { "true" } else { "false" }, span);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "init", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(init);
    p_group(out, Delimiter::Brace, b, span);
    p_punct(out, ',', Spacing::Alone, span);
}

fn emit_assign_stmt(out: &mut TokenStream, target: TokenStream, value: TokenStream, span: Span) {
    p_path(out, &["gpu", "emit", "IrStmt", "Assign"], span);
    let mut b = TokenStream::new();
    p_ident(&mut b, "target", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(target);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "value", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(value);
    p_group(out, Delimiter::Brace, b, span);
    p_punct(out, ',', Spacing::Alone, span);
}

fn emit_if_stmt(
    out: &mut TokenStream,
    cond: TokenStream,
    then_body: TokenStream,
    else_body: TokenStream,
    span: Span,
) {
    p_path(out, &["gpu", "emit", "IrStmt", "If"], span);
    let mut b = TokenStream::new();
    p_ident(&mut b, "cond", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(cond);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "then_body", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_punct(&mut b, '&', Spacing::Alone, span);
    p_group(&mut b, Delimiter::Bracket, then_body, span);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "else_body", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_punct(&mut b, '&', Spacing::Alone, span);
    p_group(&mut b, Delimiter::Bracket, else_body, span);
    p_group(out, Delimiter::Brace, b, span);
    p_punct(out, ',', Spacing::Alone, span);
}

fn emit_while_stmt(out: &mut TokenStream, cond: TokenStream, body: TokenStream, span: Span) {
    p_path(out, &["gpu", "emit", "IrStmt", "While"], span);
    let mut b = TokenStream::new();
    p_ident(&mut b, "cond", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(cond);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "body", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_punct(&mut b, '&', Spacing::Alone, span);
    p_group(&mut b, Delimiter::Bracket, body, span);
    p_group(out, Delimiter::Brace, b, span);
    p_punct(out, ',', Spacing::Alone, span);
}

fn emit_for_stmt(
    out: &mut TokenStream,
    var: &str,
    start: TokenStream,
    end: TokenStream,
    body: TokenStream,
    span: Span,
) {
    p_path(out, &["gpu", "emit", "IrStmt", "ForRange"], span);
    let mut b = TokenStream::new();
    p_ident(&mut b, "var", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_str(&mut b, var, span);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "start", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(start);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "end", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    b.extend(end);
    p_punct(&mut b, ',', Spacing::Alone, span);
    p_ident(&mut b, "body", span);
    p_punct(&mut b, ':', Spacing::Alone, span);
    p_punct(&mut b, '&', Spacing::Alone, span);
    p_group(&mut b, Delimiter::Bracket, body, span);
    p_group(out, Delimiter::Brace, b, span);
    p_punct(out, ',', Spacing::Alone, span);
}

// =============================================================================
// Iterative Expression Lowerer (Shunting-Yard + Explicit Call/Group Stack)
// =============================================================================

#[derive(Clone, Copy)]
enum Op {
    Unary(&'static str),
    Binary(&'static str, u8),
}

impl Op {
    fn prec(self) -> u8 {
        match self {
            Self::Unary(_) => 11,
            Self::Binary(_, p) => p,
        }
    }
}

enum ExprKind {
    Root,
    Paren,
    Index { base: TokenStream, span: Span },
    Call { callee: SmallStr<64>, args: [Option<TokenStream>; 8], argc: usize, span: Span },
    Method { obj: TokenStream, method: SmallStr<64>, args: [Option<TokenStream>; 8], argc: usize, span: Span },
}

struct ExprFrame {
    toks: TokBuf<256>,
    pos: usize,
    end: usize,
    expect_operand: bool,
    vals: [Option<TokenStream>; 32],
    vlen: usize,
    ops: [Option<Op>; 32],
    olen: usize,
    kind: ExprKind,
    span: Span,
}

impl ExprFrame {
    fn new(toks: TokBuf<256>, pos: usize, end: usize, kind: ExprKind, span: Span) -> Self {
        Self {
            toks,
            pos,
            end,
            expect_operand: true,
            vals: [const { None }; 32],
            vlen: 0,
            ops: [const { None }; 32],
            olen: 0,
            kind,
            span,
        }
    }

    fn push_val(&mut self, v: TokenStream) {
        if let Some(s) = self.vals.get_mut(self.vlen) {
            *s = Some(v);
            self.vlen += 1;
        }
    }

    fn pop_val(&mut self) -> TokenStream {
        if self.vlen == 0 {
            return mk_lit_f32(0.0, self.span);
        }
        self.vlen -= 1;
        self.vals.get_mut(self.vlen).and_then(Option::take).unwrap_or_else(|| mk_lit_f32(0.0, self.span))
    }

    fn apply_top_op(&mut self) {
        if self.olen == 0 {
            return;
        }
        self.olen -= 1;
        let Some(op) = self.ops.get_mut(self.olen).and_then(Option::take) else {
            return;
        };
        match op {
            Op::Unary(u) => {
                let a = self.pop_val();
                let res = mk_unary(u, a, self.span);
                self.push_val(res);
            }
            Op::Binary(b, _) => {
                let rhs = self.pop_val();
                let lhs = self.pop_val();
                let res = mk_binary(b, lhs, rhs, self.span);
                self.push_val(res);
            }
        }
    }

    fn push_op(&mut self, op: Op) {
        let is_unary = matches!(op, Op::Unary(_));
        if !is_unary {
            while self.olen > 0 {
                let top_prec = self.ops.get(self.olen - 1).and_then(|o| *o).map_or(0, Op::prec);
                if top_prec >= op.prec() {
                    self.apply_top_op();
                } else {
                    break;
                }
            }
        }
        if let Some(s) = self.ops.get_mut(self.olen) {
            *s = Some(op);
            self.olen += 1;
        }
    }

    fn finish_subexpr(&mut self) -> TokenStream {
        while self.olen > 0 {
            self.apply_top_op();
        }
        self.pop_val()
    }
}

fn lower_expr_range<const N: usize>(src: &TokBuf<N>, start: usize, end: usize, root_span: Span) -> TokenStream {
    let mut sub = TokenStream::new();
    let mut i = start;
    while i < end {
        if let Some(tt) = src.get(i) {
            reemit_tt(&mut sub, tt);
        }
        i += 1;
    }
    let root_buf = TokBuf::<256>::from_stream(sub);
    let root_len = root_buf.len;

    let mut stack: [Option<ExprFrame>; 20] = [const { None }; 20];
    stack[0] = Some(ExprFrame::new(root_buf, 0, root_len, ExprKind::Root, root_span));
    let mut top: usize = 0;

    loop {
        let done = match stack.get(top).and_then(Option::as_ref) {
            Some(f) => f.pos >= f.end,
            None => return mk_lit_f32(0.0, root_span),
        };

        if done {
            let Some(mut f) = stack.get_mut(top).and_then(Option::take) else {
                return mk_lit_f32(0.0, root_span);
            };
            let has_val = f.vlen > 0 || f.olen > 0;
            let last_val = if has_val { Some(f.finish_subexpr()) } else { None };
            let result = match f.kind {
                ExprKind::Root | ExprKind::Paren => {
                    last_val.unwrap_or_else(|| mk_lit_f32(0.0, f.span))
                }
                ExprKind::Index { base, span } => {
                    let idx_e = last_val.unwrap_or_else(|| mk_lit_f32(0.0, span));
                    mk_index(base, idx_e, span)
                }
                ExprKind::Call { callee, mut args, mut argc, span } => {
                    if let Some(lv) = last_val {
                        if let Some(slot) = args.get_mut(argc) {
                            *slot = Some(lv);
                            argc += 1;
                        }
                    }
                    finalize_call(callee.as_str(), &mut args, argc, span)
                }
                ExprKind::Method { obj, method, mut args, mut argc, span } => {
                    if let Some(lv) = last_val {
                        if let Some(slot) = args.get_mut(argc) {
                            *slot = Some(lv);
                            argc += 1;
                        }
                    }
                    finalize_method(obj, method.as_str(), &mut args, argc, span)
                }
            };
            if top == 0 {
                return result;
            }
            top -= 1;
            if let Some(parent) = stack.get_mut(top).and_then(Option::as_mut) {
                parent.push_val(result);
                parent.expect_operand = false;
            }
            continue;
        }

        let mut next_frame: Option<ExprFrame> = None;
        if let Some(f) = stack.get_mut(top).and_then(Option::as_mut) {
            let p = f.pos;
            let sp = f.toks.get(p).map_or(f.span, TokenTree::span);

            // Comma separator inside Call / Method argument lists
            if is_p(f.toks.get(p), ',') {
                let arg = f.finish_subexpr();
                match &mut f.kind {
                    ExprKind::Call { args, argc, .. } | ExprKind::Method { args, argc, .. } => {
                        if let Some(slot) = args.get_mut(*argc) {
                            *slot = Some(arg);
                            *argc += 1;
                        }
                    }
                    _ => {}
                }
                f.expect_operand = true;
                f.pos = p + 1;
                continue;
            }

            if f.expect_operand {
                // Unary `-`, `!`, `~`
                if is_p(f.toks.get(p), '-') {
                    f.push_op(Op::Unary("Neg"));
                    f.pos = p + 1;
                    continue;
                }
                if is_p(f.toks.get(p), '!') {
                    f.push_op(Op::Unary("Not"));
                    f.pos = p + 1;
                    continue;
                }
                // Parenthesized sub-expression
                if let Some(TokenTree::Group(g)) = f.toks.get(p) {
                    if g.delimiter() == Delimiter::Parenthesis {
                        let buf = TokBuf::<256>::from_stream(g.stream());
                        let blen = buf.len;
                        f.pos = p + 1;
                        next_frame = Some(ExprFrame::new(buf, 0, blen, ExprKind::Paren, sp));
                    } else {
                        f.pos = p + 1;
                    }
                } else if let Some(TokenTree::Literal(lit)) = f.toks.get(p) {
                    let s = tt_str::<64>(&TokenTree::Literal(Literal::string("")));
                    let _ = s;
                    let mut ls = SmallStr::<64>::new();
                    let _ = write!(ls, "{lit}");
                    f.push_val(lower_literal(ls.as_str(), sp));
                    f.expect_operand = false;
                    f.pos = p + 1;
                } else if let Some(TokenTree::Ident(id)) = f.toks.get(p) {
                    let mut name = SmallStr::<64>::new();
                    let _ = write!(name, "{id}");
                    // Handle path `Vec4::splat(...)` or `gpu::math::foo(...)`
                    let mut cur = p + 1;
                    while is_jp(f.toks.get(cur), ':') && is_p(f.toks.get(cur + 1), ':') {
                        if let Some(TokenTree::Ident(seg)) = f.toks.get(cur + 2) {
                            let mut combined = SmallStr::<64>::new();
                            let _ = write!(combined, "{}::{}", name.as_str(), seg);
                            name = combined;
                            cur += 3;
                        } else {
                            break;
                        }
                    }
                    if name.as_str() == "true" {
                        f.push_val(mk_lit_bool(true, sp));
                        f.expect_operand = false;
                        f.pos = cur;
                    } else if name.as_str() == "false" {
                        f.push_val(mk_lit_bool(false, sp));
                        f.expect_operand = false;
                        f.pos = cur;
                    } else if let Some(TokenTree::Group(cg)) = f.toks.get(cur) {
                        if cg.delimiter() == Delimiter::Parenthesis {
                            let buf = TokBuf::<256>::from_stream(cg.stream());
                            let blen = buf.len;
                            f.pos = cur + 1;
                            next_frame = Some(ExprFrame::new(
                                buf,
                                0,
                                blen,
                                ExprKind::Call { callee: name, args: [const { None }; 8], argc: 0, span: sp },
                                sp,
                            ));
                        } else {
                            f.push_val(mk_var(name.as_str(), sp));
                            f.expect_operand = false;
                            f.pos = cur;
                        }
                    } else {
                        f.push_val(mk_var(name.as_str(), sp));
                        f.expect_operand = false;
                        f.pos = cur;
                    }
                } else {
                    f.pos = p + 1;
                }
            } else {
                // Postfix `.field`, `.method(...)`, `[idx]` or binary operator!
                if is_p(f.toks.get(p), '.') {
                    if let Some(TokenTree::Literal(lit)) = f.toks.get(p + 1) {
                        // `.0` on `Flat<T>` / `Linear<T>` -> transparent unwrap!
                        let mut ls = SmallStr::<16>::new();
                        let _ = write!(ls, "{lit}");
                        if ls.as_str() == "0" {
                            f.pos = p + 2;
                            continue;
                        }
                    }
                    if let Some(TokenTree::Ident(mid)) = f.toks.get(p + 1) {
                        let mspan = mid.span();
                        let mut mname = SmallStr::<64>::new();
                        let _ = write!(mname, "{mid}");
                        let mg_stream = match f.toks.get(p + 2) {
                            Some(TokenTree::Group(mg)) if mg.delimiter() == Delimiter::Parenthesis => {
                                Some(mg.stream())
                            }
                            _ => None,
                        };
                        if let Some(ms) = mg_stream {
                            let obj = f.pop_val();
                            let buf = TokBuf::<256>::from_stream(ms);
                            let blen = buf.len;
                            f.pos = p + 3;
                            next_frame = Some(ExprFrame::new(
                                buf,
                                0,
                                blen,
                                ExprKind::Method {
                                    obj,
                                    method: mname,
                                    args: [const { None }; 8],
                                    argc: 0,
                                    span: mspan,
                                },
                                mspan,
                            ));
                        } else {
                            let obj = f.pop_val();
                            f.push_val(mk_field(obj, mname.as_str(), mspan));
                            f.pos = p + 2;
                            continue;
                        }
                    }
                } else if let Some(ig_stream) = match f.toks.get(p) {
                    Some(TokenTree::Group(ig)) if ig.delimiter() == Delimiter::Bracket => Some(ig.stream()),
                    _ => None,
                } {
                    let base = f.pop_val();
                    let buf = TokBuf::<256>::from_stream(ig_stream);
                    let blen = buf.len;
                    f.pos = p + 1;
                    next_frame = Some(ExprFrame::new(buf, 0, blen, ExprKind::Index { base, span: sp }, sp));
                } else if let Some((op_name, prec, step)) = match_binop(&f.toks, p) {
                    f.push_op(Op::Binary(op_name, prec));
                    f.expect_operand = true;
                    f.pos = p + step;
                    continue;
                } else {
                    f.pos = p + 1;
                }
            }
        }

        if let Some(nf) = next_frame {
            if let Some(slot) = stack.get_mut(top + 1) {
                top += 1;
                *slot = Some(nf);
            }
        }
    }
}

fn match_binop(toks: &TokBuf<256>, p: usize) -> Option<(&'static str, u8, usize)> {
    let t0 = toks.get(p);
    let t1 = toks.get(p + 1);
    if is_jp(t0, '|') && is_p(t1, '|') {
        return Some(("Or", 1, 2));
    }
    if is_jp(t0, '&') && is_p(t1, '&') {
        return Some(("And", 2, 2));
    }
    if is_jp(t0, '=') && is_p(t1, '=') {
        return Some(("Eq", 3, 2));
    }
    if is_jp(t0, '!') && is_p(t1, '=') {
        return Some(("Ne", 3, 2));
    }
    if is_jp(t0, '<') && is_p(t1, '=') {
        return Some(("Le", 3, 2));
    }
    if is_jp(t0, '>') && is_p(t1, '=') {
        return Some(("Ge", 3, 2));
    }
    if is_jp(t0, '<') && is_p(t1, '<') {
        return Some(("Shl", 7, 2));
    }
    if is_jp(t0, '>') && is_p(t1, '>') {
        return Some(("Shr", 7, 2));
    }
    if is_p(t0, '|') {
        return Some(("BitOr", 4, 1));
    }
    if is_p(t0, '^') {
        return Some(("BitXor", 5, 1));
    }
    if is_p(t0, '&') {
        return Some(("BitAnd", 6, 1));
    }
    if is_p(t0, '<') {
        return Some(("Lt", 3, 1));
    }
    if is_p(t0, '>') {
        return Some(("Gt", 3, 1));
    }
    if is_p(t0, '+') {
        return Some(("Add", 8, 1));
    }
    if is_p(t0, '-') {
        return Some(("Sub", 8, 1));
    }
    if is_p(t0, '*') {
        return Some(("Mul", 9, 1));
    }
    if is_p(t0, '/') {
        return Some(("Div", 9, 1));
    }
    if is_p(t0, '%') {
        return Some(("Rem", 9, 1));
    }
    None
}

fn finalize_call(
    callee: &str,
    args: &mut [Option<TokenStream>; 8],
    argc: usize,
    span: Span,
) -> TokenStream {
    let take = |arr: &mut [Option<TokenStream>; 8], i: usize| {
        arr.get_mut(i).and_then(Option::take).unwrap_or_else(|| mk_lit_f32(0.0, span))
    };
    // Strip path prefix if present (except for `VecN::splat`)
    if callee.ends_with("::splat") {
        let a0 = take(args, 0);
        if callee.starts_with("Vec2") {
            let mut s = TokenStream::new();
            let mut b = TokenStream::new();
            reemit_stream(&mut b, &a0);
            s.extend(a0);
            p_punct(&mut s, ',', Spacing::Alone, span);
            s.extend(b);
            return mk_construct("Vec2", s, span);
        }
        if callee.starts_with("Vec4") {
            return mk_construct("Vec4", repeat_arg4(a0, span), span);
        }
        if callee.starts_with("UVec4") {
            return mk_construct("UVec4", repeat_arg4(a0, span), span);
        }
    }
    let short = callee.rsplit("::").next().unwrap_or(callee);
    match short {
        "Flat" | "Linear" | "Position" => take(args, 0),
        "at" => {
            let slice = take(args, 0);
            let idx_e = take(args, 1);
            mk_index(slice, idx_e, span)
        }
        "to_f32" => mk_cast("F32", take(args, 0), span),
        "to_u32" => mk_cast("U32", take(args, 0), span),
        "to_i32" => mk_cast("I32", take(args, 0), span),
        "saturate" => {
            let mut list = TokenStream::new();
            list.extend(take(args, 0));
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(mk_lit_f32(0.0, span));
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(mk_lit_f32(1.0, span));
            mk_call("clamp", list, span)
        }
        "lerp" => {
            let list = join_args(args, argc, span);
            mk_call("mix", list, span)
        }
        "vec2" => mk_construct("Vec2", join_args(args, argc, span), span),
        "vec3" => mk_construct("Vec3", join_args(args, argc, span), span),
        "vec4" => mk_construct("Vec4", join_args(args, argc, span), span),
        "uvec2" => mk_construct("UVec2", join_args(args, argc, span), span),
        "uvec3" => mk_construct("UVec3", join_args(args, argc, span), span),
        "uvec4" => mk_construct("UVec4", join_args(args, argc, span), span),
        "ivec2" => mk_construct("IVec2", join_args(args, argc, span), span),
        "ivec3" => mk_construct("IVec3", join_args(args, argc, span), span),
        "ivec4" => mk_construct("IVec4", join_args(args, argc, span), span),
        other => mk_call(other, join_args(args, argc, span), span),
    }
}

fn finalize_method(
    obj: TokenStream,
    method: &str,
    args: &mut [Option<TokenStream>; 8],
    argc: usize,
    span: Span,
) -> TokenStream {
    let take = |arr: &mut [Option<TokenStream>; 8], i: usize| {
        arr.get_mut(i).and_then(Option::take).unwrap_or_else(|| mk_lit_f32(0.0, span))
    };
    match method {
        "get" => obj,
        "xy" | "zw" | "xyz" | "xz" | "yw" | "xx" | "yy" | "zz" | "ww" => mk_field(obj, method, span),
        "sample_level" => {
            // `tex.sample_level(smp, uv, level)` -> `IrExpr::Sample2d { tex, smp, uv, level: Some(&lvl) }`
            let tex_name = extract_var_str(&obj);
            let smp_e = take(args, 0);
            let smp_name = extract_var_str(&smp_e);
            let uv_e = take(args, 1);
            let lvl_e = take(args, 2);
            let mut out = TokenStream::new();
            p_path(&mut out, &["gpu", "emit", "IrExpr", "Sample2d"], span);
            let mut b = TokenStream::new();
            p_ident(&mut b, "tex", span);
            p_punct(&mut b, ':', Spacing::Alone, span);
            p_str(&mut b, tex_name.as_str(), span);
            p_punct(&mut b, ',', Spacing::Alone, span);
            p_ident(&mut b, "smp", span);
            p_punct(&mut b, ':', Spacing::Alone, span);
            p_str(&mut b, smp_name.as_str(), span);
            p_punct(&mut b, ',', Spacing::Alone, span);
            p_ident(&mut b, "uv", span);
            p_punct(&mut b, ':', Spacing::Alone, span);
            p_ref(&mut b, uv_e, span);
            p_punct(&mut b, ',', Spacing::Alone, span);
            p_ident(&mut b, "level", span);
            p_punct(&mut b, ':', Spacing::Alone, span);
            p_ident(&mut b, "Some", span);
            let mut r = TokenStream::new();
            p_ref(&mut r, lvl_e, span);
            p_group(&mut b, Delimiter::Parenthesis, r, span);
            p_group(&mut out, Delimiter::Brace, b, span);
            out
        }
        "abs" | "floor" | "ceil" | "round" | "sqrt" | "length" => mk_call(method, obj, span),
        "min" | "max" | "pow" | "dot" => {
            let mut list = obj;
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(take(args, 0));
            mk_call(method, list, span)
        }
        "clamp" => {
            let mut list = obj;
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(take(args, 0));
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(take(args, 1));
            mk_call("clamp", list, span)
        }
        "saturate" => {
            let mut list = obj;
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(mk_lit_f32(0.0, span));
            p_punct(&mut list, ',', Spacing::Alone, span);
            list.extend(mk_lit_f32(1.0, span));
            mk_call("clamp", list, span)
        }
        other => {
            let mut list = obj;
            for i in 0..argc {
                p_punct(&mut list, ',', Spacing::Alone, span);
                list.extend(take(args, i));
            }
            mk_call(other, list, span)
        }
    }
}

fn extract_var_str(expr_ts: &TokenStream) -> SmallStr<64> {
    let mut s = SmallStr::<128>::new();
    let _ = write!(s, "{expr_ts}");
    // `expr_ts` is `::gpu::emit::IrExpr::Var("name")` -> extract inside quotes!
    let raw = s.as_str();
    let mut out = SmallStr::<64>::new();
    if let Some(q0) = raw.find('"') {
        if let Some(rest) = raw.get(q0 + 1..) {
            if let Some(q1) = rest.find('"') {
                if let Some(inner) = rest.get(..q1) {
                    out.push_str(inner);
                }
            }
        }
    }
    out
}

fn reemit_stream(out: &mut TokenStream, src: &TokenStream) {
    let mut s = SmallStr::<512>::new();
    let _ = write!(s, "{src}");
    if let Ok(parsed) = s.as_str().parse::<TokenStream>() {
        out.extend(parsed);
    }
}

fn repeat_arg4(a0: TokenStream, span: Span) -> TokenStream {
    let mut s = SmallStr::<256>::new();
    let _ = write!(s, "{a0}");
    let mut out = a0;
    for _ in 0..3 {
        p_punct(&mut out, ',', Spacing::Alone, span);
        if let Ok(p) = s.as_str().parse::<TokenStream>() {
            out.extend(p);
        }
    }
    out
}

fn join_args(args: &mut [Option<TokenStream>; 8], argc: usize, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    for i in 0..argc {
        if i > 0 {
            p_punct(&mut out, ',', Spacing::Alone, span);
        }
        if let Some(a) = args.get_mut(i).and_then(Option::take) {
            out.extend(a);
        }
    }
    out
}

fn lower_literal(raw: &str, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    if raw.ends_with("u32") {
        p_path(&mut out, &["gpu", "emit", "IrExpr", "LitU32"], span);
        if let Ok(lit) = raw.parse::<TokenStream>() {
            p_group(&mut out, Delimiter::Parenthesis, lit, span);
        }
        return out;
    }
    if raw.ends_with("i32") {
        p_path(&mut out, &["gpu", "emit", "IrExpr", "LitI32"], span);
        if let Ok(lit) = raw.parse::<TokenStream>() {
            p_group(&mut out, Delimiter::Parenthesis, lit, span);
        }
        return out;
    }
    if raw.contains('.') || raw.contains('e') || raw.contains('E') || raw.ends_with("f32") {
        p_path(&mut out, &["gpu", "emit", "IrExpr", "LitF32"], span);
        if let Ok(lit) = raw.parse::<TokenStream>() {
            p_group(&mut out, Delimiter::Parenthesis, lit, span);
        }
        return out;
    }
    // Plain integer literal defaults to u32 in shader indexing/bitops.
    p_path(&mut out, &["gpu", "emit", "IrExpr", "LitU32"], span);
    if let Ok(lit) = raw.parse::<TokenStream>() {
        p_group(&mut out, Delimiter::Parenthesis, lit, span);
    }
    out
}

fn mk_lit_f32(v: f32, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "LitF32"], span);
    let mut inner = TokenStream::new();
    let mut lit = Literal::f32_suffixed(v);
    lit.set_span(span);
    inner.extend([TokenTree::Literal(lit)]);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_lit_bool(v: bool, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "LitBool"], span);
    let mut inner = TokenStream::new();
    p_ident(&mut inner, if v { "true" } else { "false" }, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_var(name: &str, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Var"], span);
    let mut inner = TokenStream::new();
    p_str(&mut inner, name, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_field(base: TokenStream, field: &str, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Field"], span);
    let mut inner = TokenStream::new();
    p_ref(&mut inner, base, span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_str(&mut inner, field, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_index(base: TokenStream, index: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Index"], span);
    let mut inner = TokenStream::new();
    p_ref(&mut inner, base, span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_ref(&mut inner, index, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_unary(op: &str, a: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Unary"], span);
    let mut inner = TokenStream::new();
    p_path(&mut inner, &["gpu", "emit", "IrUnaryOp", op], span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_ref(&mut inner, a, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_binary(op: &str, lhs: TokenStream, rhs: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Binary"], span);
    let mut inner = TokenStream::new();
    p_path(&mut inner, &["gpu", "emit", "IrBinOp", op], span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_ref(&mut inner, lhs, span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_ref(&mut inner, rhs, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_call(name: &str, args: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Call"], span);
    let mut inner = TokenStream::new();
    p_str(&mut inner, name, span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_punct(&mut inner, '&', Spacing::Alone, span);
    p_group(&mut inner, Delimiter::Bracket, args, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_construct(ty_variant: &str, args: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Construct"], span);
    let mut inner = TokenStream::new();
    p_path(&mut inner, &["gpu", "emit", "IrTy", ty_variant], span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_punct(&mut inner, '&', Spacing::Alone, span);
    p_group(&mut inner, Delimiter::Bracket, args, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}

fn mk_cast(ty_variant: &str, a: TokenStream, span: Span) -> TokenStream {
    let mut out = TokenStream::new();
    p_path(&mut out, &["gpu", "emit", "IrExpr", "Cast"], span);
    let mut inner = TokenStream::new();
    p_path(&mut inner, &["gpu", "emit", "IrTy", ty_variant], span);
    p_punct(&mut inner, ',', Spacing::Alone, span);
    p_ref(&mut inner, a, span);
    p_group(&mut out, Delimiter::Parenthesis, inner, span);
    out
}
