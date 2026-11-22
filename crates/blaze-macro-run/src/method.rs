//! The pinned bridge's methods, in `with_api!`'s order (library/proc_macro/src/bridge/mod.rs),
//! which is the order of the client's `ApiTags` and so the byte a request starts with. Moved here
//! from blaze-proc-macro protocol.rs (deleted with it at piece 2); the test reads the pinned
//! source and holds the table to it, so a toolchain bump that moves a method fails here first.

/// One method of the bridge's server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    TrackEnvVar,
    TrackPath,
    LiteralFromStr,
    EmitDiagnostic,
    TsDrop,
    TsClone,
    TsIsEmpty,
    TsExpandExpr,
    TsFromStr,
    TsToString,
    TsFromTokenTree,
    TsConcatTrees,
    TsConcatStreams,
    TsIntoTrees,
    SpanDebug,
    SpanParent,
    SpanSource,
    SpanByteRange,
    SpanStart,
    SpanEnd,
    SpanLine,
    SpanColumn,
    SpanFile,
    SpanLocalFile,
    SpanJoin,
    SpanSubspan,
    SpanResolvedAt,
    SpanSourceText,
    SpanSaveSpan,
    SpanRecoverProcMacroSpan,
    SymbolNormalizeAndValidateIdent,
}

/// Every method and its name in the pinned source, in tag order.
pub const METHODS: [(Method, &str); 31] = [
    (Method::TrackEnvVar, "track_env_var"),
    (Method::TrackPath, "track_path"),
    (Method::LiteralFromStr, "literal_from_str"),
    (Method::EmitDiagnostic, "emit_diagnostic"),
    (Method::TsDrop, "ts_drop"),
    (Method::TsClone, "ts_clone"),
    (Method::TsIsEmpty, "ts_is_empty"),
    (Method::TsExpandExpr, "ts_expand_expr"),
    (Method::TsFromStr, "ts_from_str"),
    (Method::TsToString, "ts_to_string"),
    (Method::TsFromTokenTree, "ts_from_token_tree"),
    (Method::TsConcatTrees, "ts_concat_trees"),
    (Method::TsConcatStreams, "ts_concat_streams"),
    (Method::TsIntoTrees, "ts_into_trees"),
    (Method::SpanDebug, "span_debug"),
    (Method::SpanParent, "span_parent"),
    (Method::SpanSource, "span_source"),
    (Method::SpanByteRange, "span_byte_range"),
    (Method::SpanStart, "span_start"),
    (Method::SpanEnd, "span_end"),
    (Method::SpanLine, "span_line"),
    (Method::SpanColumn, "span_column"),
    (Method::SpanFile, "span_file"),
    (Method::SpanLocalFile, "span_local_file"),
    (Method::SpanJoin, "span_join"),
    (Method::SpanSubspan, "span_subspan"),
    (Method::SpanResolvedAt, "span_resolved_at"),
    (Method::SpanSourceText, "span_source_text"),
    (Method::SpanSaveSpan, "span_save_span"),
    (Method::SpanRecoverProcMacroSpan, "span_recover_proc_macro_span"),
    (Method::SymbolNormalizeAndValidateIdent, "symbol_normalize_and_validate_ident"),
];

impl Method {
    /// The method whose request starts with `tag`.
    pub fn of(tag: u8) -> Option<Method> {
        METHODS.get(usize::from(tag)).map(|(method, _)| *method)
    }

    /// The byte a request of it starts with.
    pub fn tag(self) -> u8 {
        let mut at = 0u8;
        for (method, _) in METHODS {
            if method == self {
                return at;
            }
            at = at.saturating_add(1);
        }
        u8::MAX
    }

    /// Its name in the pinned source.
    pub fn name(self) -> &'static str {
        METHODS.get(usize::from(self.tag())).map_or("?", |(_, name)| *name)
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::{METHODS, Method};

    /// The pinned bridge's table, as the toolchain this tree builds with ships it.
    const MOD_RS: &str = include_str!("../../../library/proc_macro/src/bridge/mod.rs");

    /// The names of `with_api!`'s methods, in order: each `fn name(` inside the macro's body.
    fn pinned_names() -> Vec<&'static str> {
        let start = MOD_RS.find("macro_rules! with_api").expect("the pinned bridge has `with_api!`");
        let body = &MOD_RS[start..];
        let end = body.find("pub(crate) struct Methods").unwrap_or(body.len());
        let mut names = Vec::new();
        for line in body[..end].lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix("fn ") {
                let name = rest.split(|c: char| c == '(' || c == '<').next().expect("a method has a name");
                names.push(name.trim());
            }
        }
        names
    }

    #[test]
    fn the_table_is_the_pinned_bridges_in_order() {
        let pinned = pinned_names();
        let ours: Vec<&str> = METHODS.iter().map(|(_, name)| *name).collect();
        assert_eq!(ours, pinned);
    }

    #[test]
    fn a_tag_is_a_methods_place_and_back() {
        for (at, (method, name)) in METHODS.iter().enumerate() {
            let tag = u8::try_from(at).expect("31 methods");
            assert_eq!(Method::of(tag), Some(*method));
            assert_eq!(method.tag(), tag);
            assert_eq!(method.name(), *name);
        }
        assert_eq!(Method::of(31), None);
        // The two that reach the world are tags 0 and 1: blaze-comptime's host refuses them as
        // plain bridge calls (`method > 1`), so they come as `TrackEnv` and `TrackPath`.
        assert_eq!(Method::TrackEnvVar.tag(), 0);
        assert_eq!(Method::TrackPath.tag(), 1);
        assert_eq!(Method::SpanFile.tag(), 22);
    }
}
