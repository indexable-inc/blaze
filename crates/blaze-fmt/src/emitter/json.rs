// Follows `src/tools/rustfmt/src/emitter/json.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use super::*;
use crate::rustfmt_diff::{DiffLine, Mismatch, make_diff};
use serde::Serialize;
use serde_json::to_writer as to_json_writer;

#[derive(Debug, Default)]
pub(crate) struct JsonEmitter {
    mismatched_files: Vec<MismatchedFile>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct MismatchedBlock {
    original_begin_line: u32,
    original_end_line: u32,
    expected_begin_line: u32,
    expected_end_line: u32,
    original: String,
    expected: String,
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct MismatchedFile {
    name: String,
    mismatches: Vec<MismatchedBlock>,
}

impl JsonEmitter {
    pub(crate) fn emit_footer(&self, output: &mut impl Write) -> Result<(), io::Error> {
        to_json_writer(&mut *output, &self.mismatched_files)?;
        writeln!(output)
    }

    pub(crate) fn emit_formatted_file(
        &mut self,
        _output: &mut impl Write,
        FormattedFile {
            filename,
            original_text,
            formatted_text,
        }: FormattedFile<'_>,
    ) -> Result<EmitterResult, io::Error> {
        const CONTEXT_SIZE: usize = 0;
        let diff = make_diff(original_text, formatted_text, CONTEXT_SIZE);
        let has_diff = !diff.is_empty();

        if has_diff {
            self.add_misformatted_file(filename, diff)?;
        }

        Ok(EmitterResult { has_diff })
    }
}

impl JsonEmitter {
    fn add_misformatted_file(
        &mut self,
        filename: &FileName,
        diff: Vec<Mismatch>,
    ) -> Result<(), io::Error> {
        let mut mismatches = Vec::with_capacity(diff.len());
        for mismatch in diff {
            let original_begin_line = mismatch.line_number_orig;
            let expected_begin_line = mismatch.line_number;
            let mut original_end_line = original_begin_line;
            let mut expected_end_line = expected_begin_line;
            let mut original_line_counter = 0;
            let mut expected_line_counter = 0;
            let mut original = String::new();
            let mut expected = String::new();

            for line in mismatch.lines {
                match line {
                    DiffLine::Expected(msg) => {
                        expected_end_line = expected_begin_line + expected_line_counter;
                        expected_line_counter += 1;
                        expected.push_str(&msg);
                        expected.push('\n');
                    }
                    DiffLine::Resulting(msg) => {
                        original_end_line = original_begin_line + original_line_counter;
                        original_line_counter += 1;
                        original.push_str(&msg);
                        original.push('\n');
                    }
                    DiffLine::Context(_) => continue,
                }
            }

            mismatches.push(MismatchedBlock {
                original_begin_line,
                original_end_line,
                expected_begin_line,
                expected_end_line,
                original,
                expected,
            });
        }
        self.mismatched_files.push(MismatchedFile {
            name: format!("{filename}"),
            mismatches,
        });
        Ok(())
    }
}
