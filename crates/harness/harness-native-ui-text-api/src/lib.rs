//! What a leaf primitive shows: words, a glyph, a picture. The host's typesetter measures and shapes them.
use harness_native_flecs::prelude::*;

/// The words of a `Text` or `Label`; wraps at its laid-out width. Set from script (`Text: {s: "Hello"}`) or by feature code.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Text {
    pub s: ScriptString,
}

/// The `Text` beside it is inline markdown (a paragraph, a heading or a list item of a transcript): `**bold**`, `*italic*`,
/// `` `code` `` in the code face on a chip in `chip` (dark, light), `[links](url)` as their words in `link` (dark, light;
/// unset, the text's own ink). The markers are not shown; blocks (fences, lists) are the host's rows, not this.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MarkdownText {
    pub chip_dark: harness_native_ui_look_api::Rgba,
    pub chip_light: harness_native_ui_look_api::Rgba,
    pub link_dark: harness_native_ui_look_api::Rgba,
    pub link_light: harness_native_ui_look_api::Rgba,
}

/// Where a truncated line loses its words for the ellipsis.
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TruncateMode {
    /// "\u{2026}end of the words"
    Head,
    /// "start\u{2026}end"
    Middle,
    /// "start of the words\u{2026}"
    #[default]
    Tail,
}

/// Its `Text` is one line: when layout gives it less than its natural width (see `LayoutPriority`), the host cuts it to
/// what fits with an ellipsis at `mode`, measured by its typesetter (`harness_native_ui_layout::typeset::elide`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Truncate {
    pub mode: TruncateMode,
}

/// The `Text` beside it is set on one line, never wrapped: cut with an ellipsis to the width layout gives it (Finder's
/// sidebar rows). Its height is one line's whatever its width. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct OneLine;

/// The glyph of an `Icon`, by name in den's icon set.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Icon {
    pub glyph: ScriptString,
}

/// The mark of a `ui.Icon`, by name among the host's vector marks (harness-native's icons.rs: `claude`, `openai`,
/// `cursor`), drawn from its outline in its `Fill` at every zoom, sized optically in its box.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Mark {
    pub name: ScriptString,
}

/// A picture of an `Image`, by handle in the host's media cache.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub handle: ScriptString,
}

/// A `Picture` drawn as pixel art: each of its pixels a crisp cell of the node's size over its width and height, never
/// smoothed (CSS's `image-rendering: pixelated`). A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Pixelated;

/// A `Picture` that is a sheet of `count` frames of equal height stacked downwards, played at `fps` from the frame clock
/// and looped.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Frames {
    pub count: u32,
    pub fps: f32,
}

/// blaze's wordmark set on fire (harness-native-blaze), drawn as pixel art over the node's width: its letters start
/// falling in from the top of the view at `start` seconds on the frame clock, the fire catches a beat after the last lands,
/// and from `exit` (when it is after `start`) the letters burn away into embers. A picture without a file.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct BlazeMark {
    pub start: f32,
    pub exit: f32,
}

/// A block caret after the last glyph a node's words show (a `Pose.cut` types them on), in `ink`, from `at` to `until`
/// seconds on the frame clock: solid while glyphs are arriving, then blinking, on for half of every `blink` seconds.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Caret {
    pub at: f32,
    pub until: f32,
    pub blink: f32,
    pub ink: harness_native_ui_look_api::Rgba,
}

/// Words being edited (a `ui.TextArea`'s): the caret at UTF-16 unit `caret`, the selection from `anchor` to it, an input
/// method's marked text at `mark_lo..mark_hi` (underlined). The caret is a bar in `caret_*`, drawn while `focused`; with
/// `blink` it fades on the `Clock` as the terminal's cursor does, solid for a moment after `since` (the clock's time
/// when it last moved). The selection is a wash in `select_*` (dark, light), drawn focused or not (a transcript's
/// selection is never focused).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TextEdit {
    pub caret: u32,
    pub anchor: u32,
    pub mark_lo: u32,
    pub mark_hi: u32,
    pub focused: bool,
    pub blink: bool,
    pub since: f32,
    pub caret_dark: harness_native_ui_look_api::Rgba,
    pub caret_light: harness_native_ui_look_api::Rgba,
    pub select_dark: harness_native_ui_look_api::Rgba,
    pub select_light: harness_native_ui_look_api::Rgba,
}

/// Words that are code from `file` (a path or a name: its kind says the language): the host colours each stretch in its
/// role's ink, as it colours a code card.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Highlight {
    pub file: ScriptString,
}

/// Number the lines of a node's words from `first`, as an editor's gutter does: each line of the text (not each wrapped row)
/// gets its number, right-aligned `gap` points left of the words, in the muted ink. The room for them is the parent's (its
/// padding or the gap before the words).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct LineNumbers {
    pub first: u32,
    pub gap: f32,
}

/// A node that stands on a stretch of its parent's words (UTF-16 units `from..to` of the parent's `Text`): the host lays it
/// over those glyphs, a plate per row they cover, the way it lays a squiggle under a code card's. A merge view marks the words
/// a changed line changed with one. The node is `Placed`; it draws as a plate in its own look.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextRange {
    pub from: u32,
    pub to: u32,
}

hyperion_api_layouts::api_layouts! {
    "Text" => alias Text,
    "MarkdownText" => alias MarkdownText,
    "OneLine" => alias OneLine,
    "Icon" => alias Icon,
    "Mark" => alias Mark,
    "Picture" => alias Picture,
    "Pixelated" => alias Pixelated,
    "Frames" => alias Frames,
    "BlazeMark" => alias BlazeMark,
    "Caret" => alias Caret,
    "TextEdit" => alias TextEdit,
    "Highlight" => alias Highlight,
    "LineNumbers" => alias LineNumbers,
    "TextRange" => alias TextRange,
    "Truncate" => alias Truncate,
    TruncateMode,
}

/// Register content components before any scene resolves them.
pub fn register(world: &World) {
    world.component::<Text>().writable();
    world.component::<MarkdownText>().writable();
    world.component::<OneLine>().writable();
    world.component::<Icon>().writable();
    world.component::<Mark>().writable();
    world.component::<Picture>().writable();
    world.component::<Pixelated>().writable();
    world.component::<Frames>().writable();
    world.component::<BlazeMark>().writable();
    world.component::<Caret>().writable();
    world.component::<TextEdit>().writable();
    world.component::<Highlight>().writable();
    world.component::<LineNumbers>().writable();
    world.component::<TextRange>().writable();
    world.component::<Truncate>().writable();
}
