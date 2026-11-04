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

/// The `Text` beside it is set on one line, never wrapped: cut with an ellipsis to the width layout gives it (Finder's
/// sidebar rows). Its height is one line's whatever its width. A tag.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct OneLine;

/// The glyph of an `Icon`, by name in den's icon set.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Icon {
    pub glyph: ScriptString,
}

/// A picture of an `Image`, by handle in the host's media cache.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub handle: ScriptString,
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
    "Picture" => alias Picture,
    "Highlight" => alias Highlight,
    "LineNumbers" => alias LineNumbers,
    "TextRange" => alias TextRange,
}

/// Register content components before any scene resolves them.
pub fn register(world: &World) {
    world.component::<Text>().writable();
    world.component::<MarkdownText>().writable();
    world.component::<OneLine>().writable();
    world.component::<Icon>().writable();
    world.component::<Picture>().writable();
    world.component::<Highlight>().writable();
    world.component::<LineNumbers>().writable();
    world.component::<TextRange>().writable();
}
