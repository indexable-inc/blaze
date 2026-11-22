//! The bridge blaze's server speaks: the pinned nightly's `library/proc_macro/src/bridge`,
//! whose sources are part of this crate. A module runs only against a `proc_macro` whose
//! bridge is these sources to the byte: the protocol is not versioned, and another bridge would
//! be read as this one. The module record's key names the toolchain, and the bootstrap builder
//! refuses a toolchain whose bridge sources differ from these.

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

/// The methods by their tag, with their names in `with_api!`.
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
    pub fn of(tag: u8) -> Option<Method> {
        METHODS.get(tag as usize).map(|it| it.0)
    }

    pub fn name(self) -> &'static str {
        METHODS[self as usize].1
    }
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
        assert!(
            rpc.contains("#[cfg(target_pointer_width = \"64\")]\nrpc_encode_decode!(le usize);")
        );
        // A narrower client's `usize` is padded to eight bytes, so every client's is eight.
        assert!(rpc.contains("const MAX_USIZE_SIZE: usize = 8;"));
        assert!(rpc.contains("*r = &r[MAX_USIZE_SIZE..];"));
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
        // `guest/shim.rs`'s `Client`: one function pointer, called with the config.
        assert!(client.contains(
            "#[repr(C)]\n#[derive(Copy, Clone)]\npub struct Client {\n    pub(super) run: extern \"C\" fn(BridgeConfig<'_>) -> Buffer,\n}"
        ));
        assert!(module.contains(
            "#[repr(C)]\npub struct BridgeConfig<'a> {\n    /// Buffer used to pass initial input to the client.\n    input: Buffer,\n\n    /// Server-side function that the client uses to make requests.\n    dispatch: closure::Closure<'a>,\n\n    /// If 'true', always invoke the default panic hook\n    force_show_panics: bool,\n}"
        ));
    }
}
