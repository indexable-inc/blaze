//! The class a code-emission refusal is counted under in a survey (run.rs, `RunError::Emit`).
//!
//! A class is one cause: the refusal's variant, and for `Unsupported` the op's name, then the
//! reason the emitter gave. The instruction number, the data row and the function reference's
//! number are left out, so the same cause in two functions (or at two instructions of one) is one
//! class, and two causes behind one variant are two classes. Before this, the class was the
//! `Debug` text cut at its first `{`, which made every `Unsupported` one class whatever its op
//! and reason ("emit: Unsupported"), and every `Signature` one class ("emit: Signature").

use blaze_emit::EmitError;

/// `emit: <variant>[: <op name>][: <reason>]` for one emission refusal; never an instruction,
/// row or place number.
pub(crate) fn emit_class(error: &EmitError) -> Box<str> {
    match *error {
        EmitError::Unsupported { inst: _, op, why } => format!("emit: Unsupported: `{}`: {why}", op.name()),
        EmitError::Signature { why } => format!("emit: Signature: {why}"),
        EmitError::Data { row: _, why } => format!("emit: Data: {why}"),
        EmitError::Record { why } => format!("emit: Record: {why}"),
        EmitError::TooLarge { what } => format!("emit: TooLarge: {what}"),
        EmitError::NoVia { place: _ } => "emit: NoVia".to_owned(),
        // By the reason and the token (a mnemonic, a directive): never the instruction or line.
        EmitError::Asm { inst: _, line: _, why, token } => format!("emit: Asm: {why}: {token}"),
        // By the op and the form legalize names, or the routine: never the instruction number.
        EmitError::Legalize(why) => match why {
            blaze_lir::legalize::LegalizeError::Refused { inst: _, op, what } => format!("emit: Legalize: `{}`: {what}", op.name()),
            blaze_lir::legalize::LegalizeError::NoRoutine(r) => format!("emit: Legalize: the routine `{}` is not bound", r.symbol()),
            blaze_lir::legalize::LegalizeError::Scalable => "emit: Legalize: a scalable vector".to_owned(),
            blaze_lir::legalize::LegalizeError::Full { table } => format!("emit: Legalize: the output's {table} is full"),
            blaze_lir::legalize::LegalizeError::Verify(_) => "emit: Legalize: its output is not LIR".to_owned(),
        },
    }
    .into_boxed_str()
}

#[cfg(test)]
mod tests {
    use blaze_emit::EmitError;
    use blaze_lir::Op;

    use super::emit_class;

    /// A survey class is one cause: the variant, the op's name (`Op::name`, blaze-lir op.rs: the
    /// row `Frem = "frem"`) and the emitter's reason, with no instruction or row number. So two
    /// refusals that differ only in where they happened are one class, two that differ in op or
    /// reason are two, and the label never stops at the variant's name (the old `class_of` cut
    /// the `Debug` text at `{` and printed only "Unsupported").
    ///
    /// Expected: `test emit_class::tests::emit_class_names_op_and_reason ... ok`.
    #[test]
    fn emit_class_names_op_and_reason() {
        let at_7 = emit_class(&EmitError::Unsupported { inst: 7, op: Op::Frem, why: "x" });
        assert_eq!(&*at_7, "emit: Unsupported: `frem`: x");
        // The instruction number is not part of the class.
        let at_9 = emit_class(&EmitError::Unsupported { inst: 9, op: Op::Frem, why: "x" });
        assert_eq!(at_7, at_9);
        // Another op is another class.
        let fadd = emit_class(&EmitError::Unsupported { inst: 7, op: Op::Fadd, why: "x" });
        assert_ne!(at_7, fadd);
        assert_eq!(&*emit_class(&EmitError::Signature { why: "a `f128` parameter" }), "emit: Signature: a `f128` parameter");
        assert_eq!(&*emit_class(&EmitError::Data { row: 3, why: "y" }), "emit: Data: y");
        assert_eq!(&*emit_class(&EmitError::Data { row: 4, why: "y" }), "emit: Data: y");
        assert_eq!(&*emit_class(&EmitError::Record { why: "z" }), "emit: Record: z");
        assert_eq!(&*emit_class(&EmitError::TooLarge { what: "the frame" }), "emit: TooLarge: the frame");
        assert_eq!(&*emit_class(&EmitError::NoVia { place: 5 }), "emit: NoVia");
    }
}
