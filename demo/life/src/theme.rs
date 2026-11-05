//! Every colour and size used by the window and the editor highlighter lives in `ISLANDS_DARK`; nothing else holds a literal
//! (the cell colours, which main.rs owns, aside).
//! Palette: JetBrains "Islands Dark" (IslandSchemeDark.xml + ManyIslandsDark.theme.json in JetBrains/intellij-community;
//! the scheme's parent is Darcula, so unlisted values are Darcula's). A second theme is one more `Theme` const.

use eframe::egui::Color32;

const fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

pub struct Theme {
    /// theme.json gray-30 (MainWindow.background, the gap between islands)
    pub window_bg: Color32,
    /// theme.json gray-10 (editor / tool-window background)
    pub island_bg: Color32,
    /// scheme CARET_ROW_COLOR
    pub current_line: Color32,
    /// Darcula SELECTION_BACKGROUND
    pub selection: Color32,
    /// scheme CARET_COLOR
    pub caret: Color32,
    /// Darcula matched-brace background (editorBracketMatch in the VS Code port)
    pub bracket_match: Color32,
    /// scheme LINE_NUMBERS_COLOR
    pub gutter_fg: Color32,
    /// scheme LINE_NUMBER_ON_CARET_ROW_COLOR
    pub gutter_fg_current: Color32,
    /// TEXT / DEFAULT_IDENTIFIER / operators / punctuation / class references
    pub fg: Color32,
    /// DEFAULT_LINE_COMMENT; also muted UI text
    pub comment: Color32,
    /// DEFAULT_DOC_COMMENT
    pub doc_comment: Color32,
    /// DEFAULT_KEYWORD (also Rust builtin types, bool literals, valid escapes)
    pub keyword: Color32,
    /// DEFAULT_STRING
    pub string: Color32,
    /// DEFAULT_NUMBER
    pub number: Color32,
    /// DEFAULT_FUNCTION_DECLARATION (calls: DEFAULT_INSTANCE_METHOD 57aaf7)
    pub function: Color32,
    /// macros follow functions in the Islands scheme
    pub macro_call: Color32,
    /// DEFAULT_CONSTANT / STATIC_FIELD / INSTANCE_FIELD
    pub constant: Color32,
    /// TYPE_PARAMETER_NAME_ATTRIBUTES (also lifetimes/labels)
    pub type_param: Color32,
    /// DEFAULT_METADATA (annotations / attributes)
    pub attribute: Color32,
    /// Islands error red (theme.json red accent)
    pub error: Color32,
    /// a dead cell of the Game of Life
    pub dead_cell: Color32,
    /// tier 1, Cranelift: quick to build, so muted
    pub tier_cranelift: Color32,
    /// tier 2, LLVM -O3
    pub tier_o3: Color32,
    /// tier 3, LLVM -O3 with the profile
    pub tier_pgo: Color32,
    /// code the daemon did not compile
    pub tier_none: Color32,
    /// the empty part of a heat bar
    pub tier_track: Color32,
    pub metrics: Metrics,
}

pub struct Metrics {
    pub editor_panel_width: f32,
    pub footer_pad_v: i8,
    pub footer_pad_h: i8,
    pub header_pad_h: i8,
    pub current_line_radius: u8,
    pub editor_min_rows: usize,
    pub gutter_min_digits: usize,
    pub tab_spaces: usize,
    pub caret_width: f32,
    pub bracket_radius: u8,
    pub window_size: [f32; 2],
    /// theme.json Island.borderWidth 4-6 (outer margin per island, so ~8px between two)
    pub island_gap: i8,
    /// theme.json Island.arc 20 (= 2x radius)
    pub island_radius: u8,
    /// header island padding
    pub header_pad_v: i8,
    /// Life island padding
    pub life_pad: i8,
    /// editor island vertical padding
    pub editor_pad_v: i8,
    /// space left of the line numbers
    pub gutter_left: f32,
    /// space between line numbers and code
    pub gutter_right: f32,
    /// current-line band extends this far left over the gutter padding
    pub current_line_bleed: f32,
    /// fixed error footer, so the editor never jumps
    pub footer_height: f32,
    /// editor font size (egui's bundled Hack monospace)
    pub font_size: f32,
    /// ~1.43x
    pub line_height: f32,
    /// the tiers island under the grid, fixed so the grid never jumps
    pub tiers_height: f32,
    pub tiers_pad: i8,
    pub tier_font_size: f32,
    pub tier_row_height: f32,
    /// the level tag in front of each function
    pub tier_badge_width: f32,
    pub tier_name_width: f32,
    pub tier_bar_width: f32,
    pub tier_bar_height: f32,
    /// hot functions shown, and events
    pub tier_rows: usize,
    pub tier_events: usize,
    /// share of the island's width that the functions take; the events have the rest
    pub tier_split: f32,
}

pub const ISLANDS_DARK: Theme = Theme {
    window_bg: rgb(0x26282c), // theme.json gray-30 (MainWindow.background, the gap between islands)
    island_bg: rgb(0x191a1c), // theme.json gray-10 (editor / tool-window background)
    current_line: rgb(0x1f2024), // scheme CARET_ROW_COLOR
    selection: rgb(0x214283), // Darcula SELECTION_BACKGROUND
    caret: rgb(0xced0d6),     // scheme CARET_COLOR
    bracket_match: rgb(0x3c3f41), // Darcula matched-brace background (editorBracketMatch in the VS Code port)
    gutter_fg: rgb(0x4b5059),     // scheme LINE_NUMBERS_COLOR
    gutter_fg_current: rgb(0xa1a3ab), // scheme LINE_NUMBER_ON_CARET_ROW_COLOR
    fg: rgb(0xbcbec4), // TEXT / DEFAULT_IDENTIFIER / operators / punctuation / class references
    comment: rgb(0x7a7e85), // DEFAULT_LINE_COMMENT; also muted UI text
    doc_comment: rgb(0x5f826b), // DEFAULT_DOC_COMMENT
    keyword: rgb(0xcf8e6d), // DEFAULT_KEYWORD (also Rust builtin types, bool literals, valid escapes)
    string: rgb(0x6aab73),  // DEFAULT_STRING
    number: rgb(0x2aacb8),  // DEFAULT_NUMBER
    function: rgb(0x56a8f5), // DEFAULT_FUNCTION_DECLARATION (calls: DEFAULT_INSTANCE_METHOD 57aaf7)
    macro_call: rgb(0x56a8f5), // macros follow functions in the Islands scheme
    constant: rgb(0xc77dbb), // DEFAULT_CONSTANT / STATIC_FIELD / INSTANCE_FIELD
    type_param: rgb(0x16baac), // TYPE_PARAMETER_NAME_ATTRIBUTES (also lifetimes/labels)
    attribute: rgb(0xb3ae60), // DEFAULT_METADATA (annotations / attributes)
    error: rgb(0xf75464),   // Islands error red (theme.json red accent)
    dead_cell: rgb(0x101114), // a shade under island_bg, so the grid reads as a grid
    tier_cranelift: rgb(0x8a9bb8), // not in Islands: a grey blue, between comment and function
    tier_o3: rgb(0xe8a33e), // not in Islands: amber
    tier_pgo: rgb(0x5fb865), // not in Islands: a green a step brighter than string
    tier_none: rgb(0x6f737a), // not in Islands: between LINE_NUMBERS_COLOR and comment
    tier_track: rgb(0x26282c), // window_bg, so a bar's track reads as a slot in the island
    metrics: Metrics {
        editor_panel_width: 560.0,
        footer_pad_v: 6,
        footer_pad_h: 12,
        header_pad_h: 12,
        current_line_radius: 2,
        editor_min_rows: 30,
        gutter_min_digits: 2,
        tab_spaces: 4,
        caret_width: 1.5,
        bracket_radius: 2,
        window_size: [1280.0, 760.0],
        island_gap: 4, // theme.json Island.borderWidth 4-6 (outer margin per island, so ~8px between two)
        island_radius: 10, // theme.json Island.arc 20 (= 2x radius)
        header_pad_v: 6, // header island padding
        life_pad: 10,  // Life island padding
        editor_pad_v: 8, // editor island vertical padding
        gutter_left: 12.0, // space left of the line numbers
        gutter_right: 14.0, // space between line numbers and code
        current_line_bleed: 10.0, // current-line band extends this far left over the gutter padding
        footer_height: 78.0, // fixed error footer, so the editor never jumps
        font_size: 14.0, // editor font size (egui's bundled Hack monospace)
        line_height: 20.0, // ~1.43x
        tiers_height: 214.0, // the tiers island under the grid, fixed so the grid never jumps
        tiers_pad: 12,
        tier_font_size: 12.0,
        tier_row_height: 19.0,
        tier_badge_width: 36.0, // the level tag in front of each function
        tier_name_width: 128.0,
        tier_bar_width: 96.0,
        tier_bar_height: 6.0,
        tier_rows: 8, // hot functions shown
        tier_events: 5,
        tier_split: 0.56, // share of the island's width that the functions take
    },
};
