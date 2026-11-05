// Follows `src/tools/rustfmt/src/source_file.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::NewlineStyle;
use crate::config::FileName;
use crate::emitter::{self, Emitter};
use crate::parse::session::ParseSess;

// Append a newline to the end of each file.
pub(crate) fn append_newline(s: &mut String) {
    s.push('\n');
}

pub(crate) fn write_file<T>(
    psess: Option<&ParseSess>,
    filename: &FileName,
    formatted_text: &str,
    out: &mut T,
    emitter: &mut Emitter,
    newline_style: NewlineStyle,
) -> Result<emitter::EmitterResult, io::Error>
where
    T: Write,
{
    fn ensure_real_path(filename: &FileName) -> &Path {
        match *filename {
            FileName::Real(ref path) => path,
            _ => panic!("cannot format `{filename}` and emit to files"),
        }
    }

    // SourceFile's in the SourceMap will always have Unix-style line endings
    // See: https://github.com/rust-lang/rustfmt/issues/3850
    // So if the user has explicitly overridden the rustfmt `newline_style`
    // config and `filename` is FileName::Real, then we must check the file system
    // to get the original file value in order to detect newline_style conflicts.
    // Otherwise, parse session is around (cfg(not(test))) and newline_style has been
    // left as the default value, then try getting source from the parse session
    // source map instead of hitting the file system. This also supports getting
    // original text for `FileName::Stdin`.
    let original_text = if newline_style != NewlineStyle::Auto && *filename != FileName::Stdin {
        Arc::new(fs::read_to_string(ensure_real_path(filename))?)
    } else {
        match psess.and_then(|psess| psess.get_original_snippet(filename)) {
            Some(ori) => ori,
            None => Arc::new(fs::read_to_string(ensure_real_path(filename))?),
        }
    };

    let formatted_file = emitter::FormattedFile {
        filename,
        original_text: original_text.as_str(),
        formatted_text,
    };

    emitter.emit_formatted_file(out, formatted_file)
}
