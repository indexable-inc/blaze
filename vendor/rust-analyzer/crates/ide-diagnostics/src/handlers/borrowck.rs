use syntax::{TextRange, TextSize};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: borrowck
//
// An error of rustc's borrow checker: a use of a moved or uninitialized value, a mutation of
// what is not mutable, a borrow that conflicts with another or does not live long enough. Also
// rustc's `unused_mut` lint, which its borrow checker gives: a variable declared `mut` that is
// never mutated.
pub(crate) fn borrowck_error(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckError,
) -> Diagnostic {
    // rustc gives some errors no code, as "lifetime may not live long enough".
    let code = match d.code {
        "" => DiagnosticCode::Ra("borrowck", crate::Severity::Error),
        "unused_mut" => DiagnosticCode::RustcLint("unused_mut"),
        code => DiagnosticCode::RustcHardError(code),
    };
    let mut diagnostic = Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node);
    // rustc's span of an expression a macro writes, `&$e`, is in the macro's definition, where
    // its first token is; one the caller writes is the caller's. A definition in another file
    // stays at the call.
    if let Some(file) = d.node.file_id.macro_file() {
        let start = d.node.value.text_range().start();
        let (origin, context) = hir::InMacroFile::new(file, start).original_file_range(ctx.sema.db);
        if !context.is_root() && origin.file_id.file_id(ctx.sema.db) == diagnostic.range.file_id {
            diagnostic.range.range = origin.range;
        }
    }
    if d.at_end {
        let range = diagnostic.range.range;
        if let Some(last) = range.end().checked_sub(TextSize::new(1))
            && last >= range.start()
        {
            diagnostic.range.range = TextRange::new(last, range.end());
        }
    }
    diagnostic.stable()
}

// Diagnostic: borrowck-skipped
//
// The borrow checker did not check a body, because MIR lowering does not support something
// in it. Reported only for the borrow checker's test harness.
pub(crate) fn borrowck_skipped(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckSkipped,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("borrowck-skipped", crate::Severity::WeakWarning),
        format!("not borrow checked: {}", d.reason),
        d.node,
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn impl_trait_captures_elided_lifetime_2024() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2024
struct S(i32);
impl S {
    fn get(&self) -> impl Sized { &self.0 }
    fn kept(&self) -> impl Sized + use<> {}
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let kept = s.kept();
    s.set();
    consume(kept);
}
"#,
        );
    }

    #[test]
    fn impl_trait_captures_elided_lifetime_2021() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2021
struct S(i32);
impl S {
    fn get(&self) -> impl Sized + '_ { &self.0 }
    fn copied(&self) -> impl Sized { self.0 }
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let copied = s.copied();
    s.set();
    consume(copied);
}
"#,
        );
    }

    #[test]
    fn higher_ranked_bound_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn
trait Deserialize<'de> {}
trait DeserializeOwned: for<'de> Deserialize<'de> {}
impl<T> DeserializeOwned for T where T: for<'de> Deserialize<'de> {}
impl<'de: 'a, 'a> Deserialize<'de> for &'a str {}
impl<'de> Deserialize<'de> for u32 {}
fn owned<T: DeserializeOwned>() {}
trait Write {}
fn handler(_: &u32, _: &mut dyn Write) {}
fn run(_: impl FnMut(&u32, &mut dyn Write)) {}
fn main() {
    owned::<u32>();
    owned::<&'static str>();
  //^^^^^^^^^^^^^^^^^^^^^^^ error: higher-ranked lifetime error
    run(handler);
}
"#,
        );
    }

    #[test]
    fn unit_expression_initializes_its_place() {
        check_diagnostics(
            r#"
//- minicore: copy
#![deny(unused_mut)]
fn f() -> () {
    let mut x: i32;
    let z: () = x = 10;
    let w = z;
    let mut q: () = x = 11;
    q = x += 1;
    let mut b: () = { x += 1; };
    b = ();
    let mut l: () = loop { break; };
    l = ();
    let mut i: () = if x > 0 { x = 1 };
    i = ();
    let _ = (q, b, l, i, x);
    w
}
"#,
        );
    }

    #[test]
    fn unused_mut_self() {
        check_diagnostics(
            r#"
#![deny(unused_mut)]
struct S(u8);
impl S {
    fn read(mut self) -> u8 { self.0 }
          //^^^^^^^^ error: variable does not need to be mutable
    fn write(mut self) -> u8 { self.0 = 1; self.0 }
}
"#,
        );
    }
}
