// Follows `src/tools/rustfmt/src/emitter.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

pub(crate) use self::checkstyle::*;
pub(crate) use self::diff::*;
pub(crate) use self::files::*;
pub(crate) use self::files_with_backup::*;
pub(crate) use self::json::*;
pub(crate) use self::modified_lines::*;
pub(crate) use self::stdout::*;
use crate::config::{Config, EmitMode};
use crate::FileName;
use std::io::{self, Write};
use std::path::Path;

mod checkstyle;
mod diff;
mod files;
mod files_with_backup;
mod json;
mod modified_lines;
mod stdout;
#[cfg(test)]
mod tests;

pub(crate) struct FormattedFile<'a> {
    pub(crate) filename: &'a FileName,
    pub(crate) original_text: &'a str,
    pub(crate) formatted_text: &'a str,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct EmitterResult {
    pub(crate) has_diff: bool,
}

/// The session's emitter, one variant per emit mode.
pub(crate) enum Emitter {
    Files(FilesEmitter),
    FilesWithBackup(FilesWithBackupEmitter),
    Stdout(StdoutEmitter),
    Json(JsonEmitter),
    ModifiedLines(ModifiedLinesEmitter),
    Checkstyle(CheckstyleEmitter),
    Diff(DiffEmitter),
}

impl Emitter {
    pub(crate) fn new(config: &Config) -> Emitter {
        match config.emit_mode() {
            EmitMode::Files if config.make_backup() => {
                Emitter::FilesWithBackup(FilesWithBackupEmitter::default())
            }
            EmitMode::Files => {
                Emitter::Files(FilesEmitter::new(config.print_misformatted_file_names()))
            }
            EmitMode::Stdout | EmitMode::Coverage => {
                Emitter::Stdout(StdoutEmitter::new(config.verbose()))
            }
            EmitMode::Json => Emitter::Json(JsonEmitter::default()),
            EmitMode::ModifiedLines => Emitter::ModifiedLines(ModifiedLinesEmitter::default()),
            EmitMode::Checkstyle => Emitter::Checkstyle(CheckstyleEmitter::default()),
            EmitMode::Diff => Emitter::Diff(DiffEmitter::new(config.clone())),
        }
    }

    pub(crate) fn emit_formatted_file(
        &mut self,
        output: &mut impl Write,
        formatted_file: FormattedFile<'_>,
    ) -> Result<EmitterResult, io::Error> {
        match self {
            Emitter::Files(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::FilesWithBackup(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::Stdout(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::Json(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::ModifiedLines(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::Checkstyle(e) => e.emit_formatted_file(output, formatted_file),
            Emitter::Diff(e) => e.emit_formatted_file(output, formatted_file),
        }
    }

    pub(crate) fn emit_header(&self, output: &mut impl Write) -> Result<(), io::Error> {
        match self {
            Emitter::Checkstyle(e) => e.emit_header(output),
            Emitter::Files(_)
            | Emitter::FilesWithBackup(_)
            | Emitter::Stdout(_)
            | Emitter::Json(_)
            | Emitter::ModifiedLines(_)
            | Emitter::Diff(_) => Ok(()),
        }
    }

    pub(crate) fn emit_footer(&self, output: &mut impl Write) -> Result<(), io::Error> {
        match self {
            Emitter::Checkstyle(e) => e.emit_footer(output),
            Emitter::Json(e) => e.emit_footer(output),
            Emitter::Files(_)
            | Emitter::FilesWithBackup(_)
            | Emitter::Stdout(_)
            | Emitter::ModifiedLines(_)
            | Emitter::Diff(_) => Ok(()),
        }
    }
}

fn ensure_real_path(filename: &FileName) -> &Path {
    match *filename {
        FileName::Real(ref path) => path,
        _ => panic!("cannot format `{filename}` and emit to files"),
    }
}
