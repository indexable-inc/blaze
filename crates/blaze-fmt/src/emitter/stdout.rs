// Follows `src/tools/rustfmt/src/emitter/stdout.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use super::*;
use crate::config::Verbosity;

#[derive(Debug)]
pub(crate) struct StdoutEmitter {
    verbosity: Verbosity,
}

impl StdoutEmitter {
    pub(crate) fn new(verbosity: Verbosity) -> Self {
        Self { verbosity }
    }

    pub(crate) fn emit_formatted_file(
        &mut self,
        output: &mut impl Write,
        FormattedFile {
            filename,
            formatted_text,
            ..
        }: FormattedFile<'_>,
    ) -> Result<EmitterResult, io::Error> {
        if self.verbosity != Verbosity::Quiet {
            writeln!(output, "{filename}:\n")?;
        }
        write!(output, "{formatted_text}")?;
        Ok(EmitterResult::default())
    }
}
