// Follows `src/tools/rustfmt/src/emitter/diff.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use super::*;
use crate::config::Config;
use crate::rustfmt_diff::{make_diff, print_diff};

pub(crate) struct DiffEmitter {
    config: Config,
}

impl DiffEmitter {
    pub(crate) fn new(config: Config) -> Self {
        Self { config }
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
        const CONTEXT_SIZE: usize = 3;
        let mismatch = make_diff(original_text, formatted_text, CONTEXT_SIZE);
        let has_diff = !mismatch.is_empty();

        if has_diff {
            if self.config.print_misformatted_file_names() {
                writeln!(output, "{filename}")?;
            } else {
                print_diff(
                    mismatch,
                    |line_num| format!("Diff in {}:{}:", filename, line_num),
                    &self.config,
                );
            }
        } else if original_text != formatted_text {
            // This occurs when the only difference between the original and formatted values
            // is the newline style. This happens because The make_diff function compares the
            // original and formatted values line by line, independent of line endings.
            writeln!(output, "Incorrect newline style in {filename}")?;
            return Ok(EmitterResult { has_diff: true });
        }

        Ok(EmitterResult { has_diff })
    }
}
