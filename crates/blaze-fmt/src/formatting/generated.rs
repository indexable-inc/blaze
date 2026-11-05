// Follows `src/tools/rustfmt/src/formatting/generated.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use crate::Config;

/// Returns `true` if the given span is a part of generated files.
pub(super) fn is_generated_file(original_snippet: &str, config: &Config) -> bool {
    original_snippet
        .lines()
        // looking for marker only in the beginning of the file
        .take(config.generated_marker_line_search_limit())
        .any(|line| line.contains("@generated"))
}
