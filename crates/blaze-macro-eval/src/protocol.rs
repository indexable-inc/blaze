//! The bridge blaze's server speaks: the pinned nightly's `library/proc_macro/src/bridge`,
//! whose sources are part of this crate. A proc-macro crate runs only against a `proc_macro`
//! whose bridge is these sources to the byte ([`check`]): the protocol is not versioned, and
//! another bridge would be read as this one.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};

macro_rules! sources {
    ($($name:literal),* $(,)?) => {
        /// Each file of the bridge, by its name, with its text.
        pub(crate) const SOURCES: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../../../library/proc_macro/src/bridge/", $name))),)*
        ];
    };
}

sources!(
    "arena.rs",
    "buffer.rs",
    "client.rs",
    "closure.rs",
    "fxhash.rs",
    "handle.rs",
    "mod.rs",
    "panic_message.rs",
    "rpc.rs",
    "selfless_reify.rs",
    "server.rs",
    "symbol.rs",
);

/// The server's methods, in `with_api!`'s order, which is `ApiTags`' and so the number a
/// request starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Method {
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

/// The methods by their tag, with their names in `with_api!`.
pub(crate) const METHODS: [(Method, &str); 31] = [
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
    pub(crate) fn of(tag: u8) -> Option<Method> {
        METHODS.get(tag as usize).map(|it| it.0)
    }

    pub(crate) fn name(self) -> &'static str {
        METHODS[self as usize].1
    }
}

/// The `proc_macro` crate `krate` is built against: its dependency of the standard library's.
pub(crate) fn proc_macro_of(db: &Db, krate: Crate) -> Option<Crate> {
    krate.transitive_deps(db).iter().copied().find(|it| {
        matches!(it.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::ProcMacro))
    })
}

/// Whether the bridge of `proc_macro`, a crate in `db`, is the one blaze's server speaks:
/// each of its files as [`SOURCES`] has it. Says which file is missing or differs when not.
pub(crate) fn check(db: &Db, proc_macro: Crate) -> Result<(), String> {
    let root = db.file_source_root(proc_macro.data(db).root_file_id).source_root_id(db);
    let root = db.source_root(root).source_root_ref(db);
    for &(name, text) in SOURCES {
        let found = root.iter().find(|file| {
            root.path_for_file(file).is_some_and(|path| {
                // alloc: setup, once per file per load: the path as text.
                let path = path.to_string();
                let tail = path.strip_suffix(name);
                tail.is_some_and(|it| it.ends_with("/src/bridge/") || it.ends_with("\\src\\bridge\\"))
            })
        });
        let Some(file) = found else {
            // alloc: error path
            return Err(format!(
                "its `proc_macro` has no `src/bridge/{name}`, so its bridge is not the one blaze's \
                 server speaks"
            ));
        };
        let same = db.has_file_text(file) && AsRef::<str>::as_ref(db.file_text(file).text(db)) == text;
        if !same {
            // alloc: error path
            return Err(format!(
                "its `proc_macro`'s `src/bridge/{name}` is not the pinned nightly's that blaze's \
                 server speaks (the bridge's protocol is not versioned, so no other is read)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names in `with_api!`, in order, from the bridge's `mod.rs`.
    fn api_names() -> Vec<&'static str> {
        let module = SOURCES.iter().find(|it| it.0 == "mod.rs").unwrap().1;
        let start = module.find("macro_rules! with_api").unwrap();
        let body = &module[start..];
        let end = body.find("\n    };\n}").unwrap();
        body[..end]
            .lines()
            .filter_map(|line| line.trim().strip_prefix("fn "))
            .map(|rest| &rest[..rest.find('(').unwrap()])
            .collect() // alloc: test
    }

    #[test]
    fn the_methods_are_the_bridges_in_its_order() {
        let names: Vec<&str> = METHODS.iter().map(|it| it.1).collect(); // alloc: test
        assert_eq!(names, api_names());
        for (tag, (method, _)) in METHODS.iter().enumerate() {
            assert_eq!(*method as usize, tag);
            assert_eq!(Method::of(tag as u8), Some(*method));
        }
        assert_eq!(Method::of(METHODS.len() as u8), None);
    }

    #[test]
    fn the_encodings_the_server_assumes_are_the_bridges() {
        let module = SOURCES.iter().find(|it| it.0 == "mod.rs").unwrap().1;
        let rpc = SOURCES.iter().find(|it| it.0 == "rpc.rs").unwrap().1;
        let client = SOURCES.iter().find(|it| it.0 == "client.rs").unwrap().1;
        let buffer = SOURCES.iter().find(|it| it.0 == "buffer.rs").unwrap().1;
        let closure = SOURCES.iter().find(|it| it.0 == "closure.rs").unwrap().1;
        // `wire.rs`'s tags.
        assert!(module.contains("enum Option<T> {\n        Some(t),\n        None,\n    }"));
        assert!(module.contains("enum Result<T, E> {\n        Ok(t),\n        Err(e),\n    }"));
        assert!(module.contains("Included(x),\n        Excluded(x),\n        Unbounded,"));
        assert!(rpc.contains("rpc_encode_decode!(le usize);"));
        // A request is its tag, then its arguments in order; a reply is
        // `Result<_, PanicMessage>`.
        assert!(client.contains("ApiTags::$method.encode(&mut buf, &mut ());\n                    $($arg.encode(&mut buf, &mut ());)*"));
        assert!(client.contains("Result::<_, PanicMessage>::decode(&mut &buf[..], &mut ())"));
        // `run.rs`'s layouts: the buffer's five words and the closure's two.
        assert!(buffer.contains(
            "#[repr(C)]\npub struct Buffer {\n    data: *mut u8,\n    len: usize,\n    capacity: usize,\n    reserve: extern \"C\" fn(Buffer, usize) -> Buffer,\n    drop: extern \"C\" fn(Buffer),\n}"
        ));
        assert!(closure.contains(
            "#[repr(C)]\npub(super) struct Closure<'a> {\n    call: extern \"C\" fn(*mut Env, Buffer) -> Buffer,\n    env: *mut Env,"
        ));
        assert!(module.contains(
            "#[repr(C)]\npub struct BridgeConfig<'a> {\n    /// Buffer used to pass initial input to the client.\n    input: Buffer,\n\n    /// Server-side function that the client uses to make requests.\n    dispatch: closure::Closure<'a>,\n\n    /// If 'true', always invoke the default panic hook\n    force_show_panics: bool,\n}"
        ));
    }
}
