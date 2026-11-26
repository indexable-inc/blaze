use blaze_data::Value;
use blaze_json::{Limits, read_in};
use blaze_own::{Alloc, Erased, String, Vec};

use crate::{
    ApiError, EntityId, EntityKind, EntityMeta, MAX_MESSAGE_BYTES, OSC_CODE, PROTOCOL_VERSION,
    PUA_END, PUA_START, ServerMeta, SymbolSource, TermRegistry, encode_osc_7770_json,
    format_hex_color, parse_hex_color,
};

const TEST_META: ServerMeta<'static> =
    ServerMeta { pid: 4242, socket_path: "/tmp/blaze-shell-4242.sock" };

static TEST_ALLOC: Erased<std::alloc::Global> = Erased::new(std::alloc::Global);

fn test_alloc() -> Alloc<'static> {
    TEST_ALLOC.handle()
}

fn parse_json<'a>(raw: &str, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    read_in(raw.as_bytes(), Limits::SHIPPED, alloc).expect("valid JSON response")
}

#[test]
fn pua_auto_assign_explicit_conflict_out_of_range_and_update() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);
    assert_eq!(reg.revision(), 0);

    // Auto-assigns lowest unused PUA codepoint starting at PUA_START.
    let s1 = reg
        .register_symbol(
            "flame",
            None,
            1,
            SymbolSource::Text { text: String::from_str_in("🔥", alloc) },
        )
        .expect("register flame");
    assert_eq!(s1.codepoint, PUA_START);
    assert_eq!(s1.width_cells, 1);
    assert_eq!(s1.revision, 1);

    let s2 = reg
        .register_symbol(
            "bolt",
            None,
            99, // clamped to 2
            SymbolSource::Text { text: String::from_str_in("⚡", alloc) },
        )
        .expect("register bolt");
    assert_eq!(s2.codepoint, PUA_START + 1);
    assert_eq!(s2.width_cells, 2);
    assert_eq!(s2.revision, 2);

    // Explicit PUA slot.
    let s3 = reg
        .register_symbol(
            "star",
            Some(0xE250),
            0, // clamped to 1
            SymbolSource::Text { text: String::from_str_in("★", alloc) },
        )
        .expect("register star");
    assert_eq!(s3.codepoint, 0xE250);
    assert_eq!(s3.width_cells, 1);
    assert_eq!(s3.revision, 3);

    // Conflict error when another symbol requests an occupied slot.
    let conflict = reg
        .register_symbol(
            "other",
            Some(0xE250),
            1,
            SymbolSource::Text { text: String::from_str_in("x", alloc) },
        )
        .unwrap_err();
    assert_eq!(
        conflict,
        ApiError::CodepointInUse {
            codepoint: 0xE250,
            owner: String::from_str_in("star", alloc),
        }
    );

    // Out-of-range codepoint rejected (both below PUA_START and above PUA_END).
    let low = reg
        .register_symbol(
            "low",
            Some(PUA_START - 1),
            1,
            SymbolSource::Text { text: String::from_str_in("x", alloc) },
        )
        .unwrap_err();
    assert_eq!(low, ApiError::CodepointOutOfRange { codepoint: PUA_START - 1 });

    let high = reg
        .register_symbol(
            "high",
            Some(PUA_END + 1),
            1,
            SymbolSource::Text { text: String::from_str_in("x", alloc) },
        )
        .unwrap_err();
    assert_eq!(high, ApiError::CodepointOutOfRange { codepoint: PUA_END + 1 });

    // Updating an existing symbol with `requested_cp: None` preserves its codepoint & bumps revision.
    let updated = reg
        .register_symbol(
            "star",
            None,
            2,
            SymbolSource::Text { text: String::from_str_in("🌟", alloc) },
        )
        .expect("update star");
    assert_eq!(updated.codepoint, 0xE250);
    assert_eq!(updated.width_cells, 2);
    assert_eq!(updated.revision, 4);
    assert_eq!(reg.revision(), 4);

    // Updating an existing symbol with a new free PUA slot moves it cleanly.
    let moved = reg
        .register_symbol(
            "star",
            Some(0xE251),
            2,
            SymbolSource::Text { text: String::from_str_in("🌟", alloc) },
        )
        .expect("move star");
    assert_eq!(moved.codepoint, 0xE251);
    assert!(reg.symbol_by_codepoint(0xE250).is_none());
    assert_eq!(reg.symbol_by_codepoint(0xE251).unwrap().name.as_str(), "star");

    // Unregistering frees the lowest slot so the next auto-assigned symbol reuses it.
    assert!(reg.unregister_symbol("flame"));
    assert!(!reg.unregister_symbol("flame"));
    let reused = reg
        .register_symbol(
            "spark",
            None,
            1,
            SymbolSource::Text { text: String::from_str_in("✨", alloc) },
        )
        .expect("register spark");
    assert_eq!(reused.codepoint, PUA_START);

    // Invalid symbol slug names rejected.
    for bad in ["", "has space", "bad/slash", &"a".repeat(97)] {
        let err = reg
            .register_symbol(
                bad,
                None,
                1,
                SymbolSource::Text { text: String::from_str_in("x", alloc) },
            )
            .unwrap_err();
        assert_eq!(err.code(), "invalid_field");
    }
}

#[test]
fn all_four_symbol_sources_and_base64_validation() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);

    let png_b64 = blaze_base64::STANDARD.encode_in(b"\x89PNG\r\n\x1a\nfake-png-bytes", alloc);
    let font_b64 = blaze_base64::STANDARD.encode_in(b"OTTO-fake-font-payload-1234567890", alloc);

    reg.register_symbol(
        "logo.svg",
        None,
        1,
        SymbolSource::Svg {
            markup: String::from_str_in("<svg viewBox=\"0 0 16 16\"><circle r=\"8\"/></svg>", alloc),
            monochrome: true,
        },
    )
    .expect("svg");

    reg.register_symbol(
        "nf:rust",
        None,
        1,
        SymbolSource::FontGlyph {
            font_name: Some(String::from_str_in("Symbols Nerd Font", alloc)),
            font_data_b64: Some(font_b64.clone()),
            codepoint: 0xE7A8,
            tint: Some([0xDE, 0xA5, 0x84, 0xFF]),
        },
    )
    .expect("font_glyph");

    reg.register_symbol(
        "badge_png",
        None,
        2,
        SymbolSource::Png { data_b64: png_b64.clone() },
    )
    .expect("png");

    reg.register_symbol(
        "emoji-fire",
        None,
        2,
        SymbolSource::Text { text: String::from_str_in("🔥", alloc) },
    )
    .expect("text");

    assert_eq!(reg.symbol_by_name("logo.svg").unwrap().source.kind_str(), "svg");
    assert_eq!(reg.symbol_by_name("nf:rust").unwrap().source.kind_str(), "font_glyph");
    assert_eq!(reg.symbol_by_name("badge_png").unwrap().source.kind_str(), "png");
    assert_eq!(reg.symbol_by_name("emoji-fire").unwrap().source.kind_str(), "text");

    // Invalid base64 rejected for both Png and FontGlyph.
    for bad_b64 in ["", "abc", "!!!!", "AA==AAAA", "AAB="] {
        let err_png = reg
            .register_symbol(
                "bad-png",
                None,
                1,
                SymbolSource::Png { data_b64: String::from_str_in(bad_b64, alloc) },
            )
            .unwrap_err();
        assert_eq!(err_png.code(), "invalid_base64", "bad png b64 `{bad_b64}`");

        let err_font = reg
            .register_symbol(
                "bad-font",
                None,
                1,
                SymbolSource::FontGlyph {
                    font_name: None,
                    font_data_b64: Some(String::from_str_in(bad_b64, alloc)),
                    codepoint: 0xE7A8,
                    tint: None,
                },
            )
            .unwrap_err();
        assert_eq!(err_font.code(), "invalid_base64", "bad font b64 `{bad_b64}`");
    }
}

#[test]
fn entity_id_file_lookup_compact_parse_and_hex_colors() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);

    let file_id = EntityId::file(16777234, 9812345, alloc);
    assert_eq!(file_id.kind, EntityKind::File);
    assert_eq!(file_id.id.as_str(), "16777234:9812345");
    assert_eq!(file_id.parse_file_dev_ino(), Some((16777234, 9812345)));

    let compact_file = EntityId::parse_compact("file:16777234:9812345", alloc).expect("compact file");
    assert_eq!(compact_file, file_id);

    let compact_host = EntityId::parse_compact("host:hydra", alloc).expect("compact host");
    assert_eq!(compact_host.kind, EntityKind::Host);
    assert_eq!(compact_host.id.as_str(), "hydra");
    assert_eq!(compact_host.parse_file_dev_ino(), None);

    let compact_repo = EntityId::parse_compact("repo:abc123", alloc).expect("compact repo");
    assert_eq!(compact_repo.kind, EntityKind::Repo);
    assert_eq!(compact_repo.id.as_str(), "abc123");

    assert!(EntityId::parse_compact("unknown:123", alloc).is_none());
    assert!(EntityId::parse_compact("host:", alloc).is_none());
    assert!(EntityId::parse_compact("no-colon", alloc).is_none());

    // Zero-alloc lookup via entity_file(dev, ino), including boundary u64 values.
    let mut meta = EntityMeta::new(file_id.clone_in(alloc), alloc);
    meta.display_name = Some(String::from_str_in("blaze", alloc));
    meta.symbol = Some(String::from_str_in("flame", alloc));
    meta.color = parse_hex_color("#ff8800");
    reg.bind_entity(meta).expect("bind file");

    let mut max_meta = EntityMeta::new(EntityId::file(0, u64::MAX, alloc), alloc);
    max_meta.display_name = Some(String::from_str_in("edge", alloc));
    reg.bind_entity(max_meta).expect("bind max file");

    let found = reg.entity_file(16777234, 9812345).expect("entity_file hit");
    assert_eq!(found.display_name.as_ref().unwrap().as_str(), "blaze");
    assert_eq!(found.color, Some([0xFF, 0x88, 0x00, 0xFF]));

    let found_edge = reg.entity_file(0, u64::MAX).expect("entity_file u64::MAX hit");
    assert_eq!(found_edge.display_name.as_ref().unwrap().as_str(), "edge");
    assert!(reg.entity_file(1, 2).is_none());

    // Hex color parse & format round-trip.
    assert_eq!(parse_hex_color("#12Ab34"), Some([0x12, 0xAB, 0x34, 0xFF]));
    assert_eq!(format_hex_color([0x12, 0xAB, 0x34, 0xFF], alloc).as_str(), "#12ab34");
    assert_eq!(parse_hex_color("#12ab3480"), Some([0x12, 0xAB, 0x34, 0x80]));
    assert_eq!(format_hex_color([0x12, 0xAB, 0x34, 0x80], alloc).as_str(), "#12ab3480");
    assert_eq!(parse_hex_color("123456"), None);
    assert_eq!(parse_hex_color("#12345"), None);
    assert_eq!(parse_hex_color("#12345g"), None);
}

#[test]
fn json_rpc_all_ops_and_error_paths() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);

    // ping
    let (resp, mutated) = reg.apply_json(br#"{"op":"ping"}"#, TEST_META, alloc);
    assert!(!mutated);
    let v = parse_json(resp.as_str(), alloc);
    assert_eq!(v.get("ok").unwrap().as_bool(), Some(true));
    assert_eq!(v.get("protocol").unwrap().as_u64(), Some(u64::from(PROTOCOL_VERSION)));
    assert_eq!(v.get("revision").unwrap().as_u64(), Some(0));

    // symbol.register (text, auto-assigned PUA)
    let (resp, mutated) = reg.apply_json(
        r#"{"op":"symbol.register","name":"flame","kind":"text","text":"🔥","width":2}"#.as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(mutated);
    let v = parse_json(resp.as_str(), alloc);
    assert_eq!(v.get("ok").unwrap().as_bool(), Some(true));
    let sym = v.get("symbol").unwrap();
    assert_eq!(sym.get("codepoint").unwrap().as_u64(), Some(u64::from(PUA_START)));
    assert_eq!(sym.get("codepoint_hex").unwrap().as_str(), Some("U+E100"));
    let expected_utf8 = char::from_u32(PUA_START).unwrap();
    let mut expected_str = String::new_in(alloc);
    expected_str.push(expected_utf8);
    assert_eq!(sym.get("utf8").unwrap().as_str(), Some(expected_str.as_str()));

    // symbol.register (svg, explicit U+E200)
    let (resp, mutated) = reg.apply_json(
        br#"{"op":"symbol.register","name":"logo","codepoint":"U+E200","kind":"svg","svg":"<svg/>","monochrome":true}"#,
        TEST_META,
        alloc,
    );
    assert!(mutated);
    let v = parse_json(resp.as_str(), alloc);
    assert_eq!(v.get("symbol").unwrap().get("codepoint").unwrap().as_u64(), Some(0xE200));

    // symbol.register (font_glyph with 0xE201 + base64 font)
    let font_b64 = blaze_base64::STANDARD.encode_in(b"TTF-DATA", alloc);
    let font_req = blaze_own::format_in!(
        alloc,
        r##"{{"op":"symbol.register","name":"glyph","codepoint":"0xE201","kind":"font_glyph","font_name":"Mono","font_data_b64":"{}","glyph_codepoint":"U+F120","tint":"#aabbcc80"}}"##,
        font_b64.as_str()
    );
    let (resp, mutated) = reg.apply_json(font_req.as_bytes(), TEST_META, alloc);
    assert!(mutated);
    let v = parse_json(resp.as_str(), alloc);
    assert_eq!(v.get("ok").unwrap().as_bool(), Some(true));
    assert_eq!(v.get("symbol").unwrap().get("tint").unwrap().as_str(), Some("#aabbcc80"));

    // symbol.register (png)
    let png_b64 = blaze_base64::STANDARD.encode_in(b"PNG-DATA", alloc);
    let png_req = blaze_own::format_in!(
        alloc,
        r#"{{"op":"symbol.register","name":"pic","kind":"png","data_b64":"{}"}}"#,
        png_b64.as_str()
    );
    let (resp, mutated) = reg.apply_json(png_req.as_bytes(), TEST_META, alloc);
    assert!(mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("ok").unwrap().as_bool(), Some(true));

    // symbol.get by name and by codepoint
    let (resp, mutated) =
        reg.apply_json(br#"{"op":"symbol.get","name":"logo"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(
        parse_json(resp.as_str(), alloc)
            .get("symbol")
            .unwrap()
            .get("codepoint")
            .unwrap()
            .as_u64(),
        Some(0xE200)
    );

    let (resp, mutated) =
        reg.apply_json(br#"{"op":"symbol.get","codepoint":"U+E200"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(
        parse_json(resp.as_str(), alloc)
            .get("symbol")
            .unwrap()
            .get("name")
            .unwrap()
            .as_str(),
        Some("logo")
    );

    // symbol.list (sorted by name)
    let (resp, mutated) = reg.apply_json(br#"{"op":"symbol.list"}"#, TEST_META, alloc);
    assert!(!mutated);
    let list_v = parse_json(resp.as_str(), alloc);
    let syms = list_v.get("symbols").unwrap().as_list().unwrap();
    let names: std::vec::Vec<&str> =
        syms.iter().map(|s| s.get("name").unwrap().as_str().unwrap()).collect();
    assert_eq!(names, std::vec!["flame", "glyph", "logo", "pic"]);

    // entity.bind (file via dev + ino)
    let (resp, mutated) = reg.apply_json(
        r##"{"op":"entity.bind","kind":"file","dev":10,"ino":20,"symbol":"flame","display_name":"Blaze","color":"#ff5500","badge":"v2","hint_path":"/Volumes/Projects/blaze","attrs":{"b":"2","a":"1"}}"##
            .as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(mutated);
    let ent_v = parse_json(resp.as_str(), alloc);
    assert_eq!(ent_v.get("ok").unwrap().as_bool(), Some(true));
    assert_eq!(ent_v.get("entity").unwrap().get("id").unwrap().as_str(), Some("10:20"));
    assert_eq!(ent_v.get("entity").unwrap().get("dev").unwrap().as_u64(), Some(10));
    assert_eq!(ent_v.get("entity").unwrap().get("ino").unwrap().as_u64(), Some(20));

    // entity.bind (host via id)
    let (resp, mutated) = reg.apply_json(
        r#"{"op":"entity.bind","kind":"host","id":"hydra","symbol":"🦑","display_name":"Hydra"}"#
            .as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("ok").unwrap().as_bool(), Some(true));

    // entity.get (file via dev + ino)
    let (resp, mutated) =
        reg.apply_json(br#"{"op":"entity.get","kind":"file","dev":10,"ino":20}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(
        parse_json(resp.as_str(), alloc)
            .get("entity")
            .unwrap()
            .get("display_name")
            .unwrap()
            .as_str(),
        Some("Blaze")
    );

    // entity.list (all and filtered)
    let (resp, mutated) = reg.apply_json(br#"{"op":"entity.list"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(
        parse_json(resp.as_str(), alloc).get("entities").unwrap().as_list().unwrap().len(),
        2
    );

    let (resp, mutated) =
        reg.apply_json(br#"{"op":"entity.list","kind":"host"}"#, TEST_META, alloc);
    assert!(!mutated);
    let hosts = parse_json(resp.as_str(), alloc);
    assert_eq!(hosts.get("entities").unwrap().as_list().unwrap().len(), 1);
    assert_eq!(
        hosts.get("entities").unwrap().at(0).unwrap().get("id").unwrap().as_str(),
        Some("hydra")
    );

    // info
    let (resp, mutated) = reg.apply_json(br#"{"op":"info"}"#, TEST_META, alloc);
    assert!(!mutated);
    let info = parse_json(resp.as_str(), alloc);
    assert_eq!(info.get("pid").unwrap().as_u64(), Some(4242));
    assert_eq!(info.get("socket").unwrap().as_str(), Some("/tmp/blaze-shell-4242.sock"));
    assert_eq!(info.get("symbols").unwrap().as_u64(), Some(4));
    assert_eq!(info.get("entities").unwrap().as_u64(), Some(2));
    assert_eq!(info.get("pua_range").unwrap().at(0).unwrap().as_str(), Some("U+E100"));
    assert_eq!(info.get("pua_range").unwrap().at(1).unwrap().as_str(), Some("U+EFFF"));

    // entity.unbind
    let (resp, mutated) = reg.apply_json(
        br#"{"op":"entity.unbind","kind":"file","dev":10,"ino":20}"#,
        TEST_META,
        alloc,
    );
    assert!(mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("removed").unwrap().as_bool(), Some(true));

    let (resp, mutated) = reg.apply_json(
        br#"{"op":"entity.unbind","kind":"file","dev":10,"ino":20}"#,
        TEST_META,
        alloc,
    );
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("removed").unwrap().as_bool(), Some(false));

    // symbol.unregister
    let (resp, mutated) =
        reg.apply_json(br#"{"op":"symbol.unregister","name":"pic"}"#, TEST_META, alloc);
    assert!(mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("removed").unwrap().as_bool(), Some(true));

    // Error paths
    let (resp, mutated) = reg.apply_json(b"{not json", TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(), Some("invalid_json"));

    let (resp, mutated) = reg.apply_json(br#"{"op":"nope"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(), Some("unknown_op"));

    let (resp, mutated) =
        reg.apply_json(br#"{"op":"symbol.get","name":"missing"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(), Some("not_found"));

    let (resp, mutated) =
        reg.apply_json(br#"{"op":"entity.get","kind":"host","id":"missing"}"#, TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(), Some("not_found"));

    let (resp, mutated) = reg.apply_json(
        br#"{"op":"entity.bind","kind":"file","id":"1:2","dev":1,"ino":3}"#,
        TEST_META,
        alloc,
    );
    assert!(!mutated);
    assert_eq!(
        parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(),
        Some("invalid_field")
    );

    // TooLarge check
    let mut oversized = Vec::with_capacity_in(MAX_MESSAGE_BYTES + 1, alloc);
    oversized.resize(MAX_MESSAGE_BYTES + 1, b' ');
    let (resp, mutated) = reg.apply_json(oversized.as_slice(), TEST_META, alloc);
    assert!(!mutated);
    assert_eq!(parse_json(resp.as_str(), alloc).get("code").unwrap().as_str(), Some("too_large"));
}

#[test]
fn osc_7770_json_and_compact_kv_modes() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);

    // 1. JSON mode without reply
    let out1 = reg.apply_osc_7770(
        r#"{"op":"symbol.register","name":"rocket","kind":"text","text":"🚀"}"#.as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(out1.mutated);
    assert!(out1.reply_osc.is_none());
    assert_eq!(parse_json(out1.response_json.as_str(), alloc).get("ok").unwrap().as_bool(), Some(true));

    // 2. JSON mode with reply: true
    let out2 = reg.apply_osc_7770(
        br#"{"op":"ping","reply":true}"#,
        TEST_META,
        alloc,
    );
    assert!(!out2.mutated);
    let expected_reply = encode_osc_7770_json(out2.response_json.as_str(), alloc);
    assert_eq!(out2.reply_osc.as_ref(), Some(&expected_reply));
    assert!(expected_reply.as_str().starts_with(&std::format!("\x1b]{OSC_CODE};")));
    assert!(expected_reply.as_str().ends_with("\x1b\\"));

    // 3. Compact k=v mode: symbol.register with inferred kind=text
    let out3 = reg.apply_osc_7770(
        "op=symbol.register;name=flame;text=🔥".as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(out3.mutated);
    assert!(out3.reply_osc.is_none());
    assert_eq!(reg.symbol_by_name("flame").unwrap().source.kind_str(), "text");

    // 4. Compact k=v mode: entity.bind with dev+ino, name->display_name, color, attr.*, reply=true
    let out4 = reg.apply_osc_7770(
        "op=entity.bind;kind=file;dev=1;ino=2;symbol=flame;name=MyDir;color=#33aa99;attr.vcs=git;reply=true"
            .as_bytes(),
        TEST_META,
        alloc,
    );
    assert!(out4.mutated);
    assert!(out4.reply_osc.is_some());
    let ent = reg.entity_file(1, 2).expect("bound file entity via OSC 7770");
    assert_eq!(ent.symbol.as_ref().unwrap().as_str(), "flame");
    assert_eq!(ent.display_name.as_ref().unwrap().as_str(), "MyDir");
    assert_eq!(ent.color, Some([0x33, 0xAA, 0x99, 0xFF]));
    assert_eq!(ent.attrs.len(), 1);
    assert_eq!(ent.attrs[0].0.as_str(), "vcs");
    assert_eq!(ent.attrs[0].1.as_str(), "git");

    // 5. Compact k=v error handling (missing '=')
    let bad = reg.apply_osc_7770(b"op=ping;broken_segment", TEST_META, alloc);
    assert!(!bad.mutated);
    assert!(bad.reply_osc.is_none());
    assert_eq!(
        parse_json(bad.response_json.as_str(), alloc).get("code").unwrap().as_str(),
        Some("invalid_field")
    );
}

#[test]
fn canonical_snapshot_exact_deterministic_roundtrip() {
    let alloc = test_alloc();
    let mut reg = TermRegistry::new_in(alloc);

    let png_b64 = blaze_base64::STANDARD.encode_in(b"\x89PNG-payload", alloc);
    let font_b64 = blaze_base64::STANDARD.encode_in(b"OTTO-font", alloc);

    // Insert symbols and entities out of alphabetical order.
    reg.register_symbol(
        "zeta",
        Some(0xE300),
        2,
        SymbolSource::Svg {
            markup: String::from_str_in("<svg><rect/></svg>", alloc),
            monochrome: false,
        },
    )
    .unwrap();
    reg.register_symbol(
        "alpha",
        None,
        1,
        SymbolSource::FontGlyph {
            font_name: Some(String::from_str_in("NerdFont", alloc)),
            font_data_b64: Some(font_b64),
            codepoint: 0xE7A8,
            tint: Some([0x10, 0x20, 0x30, 0x40]),
        },
    )
    .unwrap();
    reg.register_symbol("mid", None, 1, SymbolSource::Png { data_b64: png_b64 })
        .unwrap();

    let mut e_host = EntityMeta::new(EntityId::new(EntityKind::Host, "hydra", alloc), alloc);
    e_host.symbol = Some(String::from_str_in("alpha", alloc));
    e_host.display_name = Some(String::from_str_in("Hydra", alloc));
    e_host.attrs.push((String::from_str_in("z", alloc), String::from_str_in("last", alloc)));
    e_host.attrs.push((String::from_str_in("a", alloc), String::from_str_in("first", alloc)));
    reg.bind_entity(e_host).unwrap();

    let mut e_file = EntityMeta::new(EntityId::file(99, 1234, alloc), alloc);
    e_file.symbol = Some(String::from_str_in("zeta", alloc));
    e_file.color = Some([0xFF, 0x00, 0xAA, 0xFF]);
    e_file.badge = Some(String::from_str_in("main", alloc));
    e_file.hint_path = Some(String::from_str_in("/repo", alloc));
    reg.bind_entity(e_file).unwrap();

    let snap1 = reg.encode_snapshot(alloc);
    let decoded = TermRegistry::decode_snapshot(snap1.as_bytes(), alloc).expect("decode snapshot");
    let snap2 = decoded.encode_snapshot(alloc);

    // Exact byte-for-byte deterministic round-trip!
    assert_eq!(snap1.as_str(), snap2.as_str());
    assert_eq!(decoded.revision(), reg.revision());
    assert_eq!(decoded.symbols().len(), 3);
    assert_eq!(decoded.entities().len(), 2);

    // Verify canonical sorted order in snapshot: symbols alpha, mid, zeta; entities file then host.
    let v = parse_json(snap1.as_str(), alloc);
    assert_eq!(v.get("version").unwrap().as_u64(), Some(u64::from(PROTOCOL_VERSION)));
    assert_eq!(v.get("symbols").unwrap().at(0).unwrap().get("name").unwrap().as_str(), Some("alpha"));
    assert_eq!(v.get("symbols").unwrap().at(1).unwrap().get("name").unwrap().as_str(), Some("mid"));
    assert_eq!(v.get("symbols").unwrap().at(2).unwrap().get("name").unwrap().as_str(), Some("zeta"));
    assert_eq!(v.get("entities").unwrap().at(0).unwrap().get("kind").unwrap().as_str(), Some("file"));
    assert_eq!(v.get("entities").unwrap().at(1).unwrap().get("kind").unwrap().as_str(), Some("host"));

    // Invalid snapshot version rejected.
    let bad_ver = TermRegistry::decode_snapshot(
        br#"{"version":99,"revision":0,"symbols":[],"entities":[]}"#,
        alloc,
    )
    .unwrap_err();
    assert_eq!(bad_ver.code(), "invalid_field");

    // Duplicate codepoint in snapshot rejected.
    let dup_cp = TermRegistry::decode_snapshot(
        br#"{"version":1,"revision":2,"symbols":[{"name":"a","codepoint":57600,"width":1,"kind":"text","text":"a","revision":1},{"name":"b","codepoint":57600,"width":1,"kind":"text","text":"b","revision":2}],"entities":[]}"#,
        alloc,
    )
    .unwrap_err();
    assert_eq!(dup_cp.code(), "codepoint_in_use");
}
