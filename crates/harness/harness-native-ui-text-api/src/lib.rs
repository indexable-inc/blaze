//! What a leaf primitive shows: words, a glyph, a picture. The host's typesetter measures and shapes them.
use harness_native_flecs::prelude::*;

/// The words of a `Text` or `Label`; wraps at its laid-out width. Set from script (`Text: {s: "Hello"}`) or by feature code.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Text {
    pub s: ScriptString,
}

/// The `Text` beside it is markdown, set as the transcript sets prose (the host's `markdown::render_words`): code spans on
/// a chip, code blocks coloured by their language on a card reaching `card_x` past the words either side and `card_y` into
/// the blank lines around it, headings in the bold of the text's own size. Inside a `Scroll`, its words fade over `fade`
/// points where the scroll cuts them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MarkdownText {
    pub card_x: f32,
    pub card_y: f32,
    pub fade: f32,
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
    world.component::<Highlight>().writable();
    world.component::<LineNumbers>().writable();
    world.component::<TextRange>().writable();
    world.component::<Truncate>().writable();
}
