// Follows `src/tools/rustfmt/src/emitter/checkstyle.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use self::xml::XmlEscaped;
use super::*;
use crate::rustfmt_diff::{DiffLine, Mismatch, make_diff};

mod xml;

#[derive(Debug, Default)]
pub(crate) struct CheckstyleEmitter;

impl CheckstyleEmitter {
    pub(crate) fn emit_header(&self, output: &mut impl Write) -> Result<(), io::Error> {
        writeln!(output, r#"<?xml version="1.0" encoding="utf-8"?>"#)?;
        write!(output, r#"<checkstyle version="4.3">"#)?;
        Ok(())
    }

    pub(crate) fn emit_footer(&self, output: &mut impl Write) -> Result<(), io::Error> {
        writeln!(output, "</checkstyle>")
    }

    pub(crate) fn emit_formatted_file(
        &mut self,
        output: &mut impl Write,
        FormattedFile {
            filename,
            original_text,
            formatted_text,
        }: FormattedFile<'_>,
    ) -> Result<EmitterResult, io::Error> {
        const CONTEXT_SIZE: usize = 0;
        let diff = make_diff(original_text, formatted_text, CONTEXT_SIZE);
        output_checkstyle_file(output, filename, diff)?;
        Ok(EmitterResult::default())
    }
}

pub(crate) fn output_checkstyle_file<T>(
    mut writer: T,
    filename: &FileName,
    diff: Vec<Mismatch>,
) -> Result<(), io::Error>
where
    T: Write,
{
    write!(writer, r#"<file name="{filename}">"#)?;
    for mismatch in diff {
        let begin_line = mismatch.line_number;
        let mut current_line;
        let mut line_counter = 0;
        for line in mismatch.lines {
            // Do nothing with `DiffLine::Context` and `DiffLine::Resulting`.
            if let DiffLine::Expected(message) = line {
                current_line = begin_line + line_counter;
                line_counter += 1;
                write!(
                    writer,
                    r#"<error line="{}" severity="warning" message="Should be `{}`" />"#,
                    current_line,
                    XmlEscaped(&message)
                )?;
            }
        }
    }
    write!(writer, "</file>")?;
    Ok(())
}
