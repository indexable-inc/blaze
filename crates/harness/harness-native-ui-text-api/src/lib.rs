//! What a leaf primitive shows: words, a glyph, a picture. The host's typesetter measures and shapes them.
use harness_native_flecs::prelude::*;

/// The words of a `Text` or `Label`; wraps at its laid-out width. Set from script (`Text: {s: "Hello"}`) or by feature code.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Text {
    pub s: ScriptString,
}

/// The `Text` beside it is inline markdown (a paragraph, a heading or a list item of a transcript): `**bold**`, `*italic*`,
/// `` `code` `` in the code face on a chip in `chip` (dark, light) `chip_radius` round (0: the painter's
/// `markdown::CHIP_RADIUS`), its glyphs in `code` (dark, light), `[links](url)` as their words in `link`, bold in `strong`.
/// An ink left unset (alpha 0) is the text's own. The markers are not shown; blocks (fences, lists) are the host's rows.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MarkdownText {
    pub chip_dark: harness_native_ui_look_api::Rgba,
    pub chip_light: harness_native_ui_look_api::Rgba,
    pub link_dark: harness_native_ui_look_api::Rgba,
    pub link_light: harness_native_ui_look_api::Rgba,
    pub code_dark: harness_native_ui_look_api::Rgba,
    pub code_light: harness_native_ui_look_api::Rgba,
    pub strong_dark: harness_native_ui_look_api::Rgba,
    pub strong_light: harness_native_ui_look_api::Rgba,
    pub chip_radius: f32,
}

/// One inline component of inline markdown (`<Path loc="...">blaze</Path>`, harness-native-ui-inline's grammar): an entity
/// of the `ui.inline` module per name (`ui.inline.Path`), drawn as a chip of its `glyph` and its label. `target` names the
/// prop that says where a press goes (`loc`, `href`; empty: the chip opens nothing) and `line` the prop with a line in it
/// (empty: none); with `file_icon` the icon is the drawing of the target's file kind (harness-native-ui-inline's
/// files.rs, in the drawing's own colours), not `glyph`.
/// The glyph lives here, not in an `Icon`, because inside `ui.inline` the name `Icon` resolves to the `ui.Icon` prefab.
///
/// The rest are formats over the tag's props (`{n}` is its `n`; harness-native-ui-inline's `format`), each unused when
/// empty: `label` what a self-closing tag shows (`#{n}`; empty: its first prop's value), `href` where a press opens in
/// place of the `target` prop (`https://github.com/{repo}/pull/{n}`), `key` the name its live state goes by
/// ([`InlineLive`]; empty: it has none), and `feed` which of the host's feeds keeps that state (`github.pr`). `mark`: room
/// after its label for a small mark its live state picks ([`InlineMark`]: a CI check). `url` names the prop a link keeps
/// its web address in (`ui.inline.Link`: `href`): a link whose address another component's `href` fits is that component
/// (a link to a pull request is its `PR` chip), and one that stays a link shows its host after its words.
///
/// A component with live state has a child per state it can be in (`ui.inline.PR.merged`), each with its own `InlineRef`
/// (its `glyph`, or `file_icon`) and `Fill` (the glyph's ink), and an `unknown` child for while nothing is known. On a
/// file component's state, `folder` says the host found the target a folder (`ui.inline.Path.folder`), so its icon is a
/// folder's (by the folder's name: `.github` has its own) whatever its name says.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct InlineRef {
    pub target: ScriptString,
    pub line: ScriptString,
    pub file_icon: bool,
    pub glyph: ScriptString,
    pub label: ScriptString,
    pub href: ScriptString,
    pub key: ScriptString,
    pub feed: ScriptString,
    pub mark: bool,
    pub url: ScriptString,
    pub folder: bool,
    /// Not a chip but the words themselves, cited (`ui.inline.Cite`): no icon, set in the face of the words around them,
    /// a dotted underline under them at rest and a solid one under the pointer.
    pub plain: bool,
}

/// What the host knows of the thing a live inline component names (a pull request), on an entity of its own the host's
/// feed keeps: `key` as the component's `InlineRef.key` formats it, `tip` what hovering the chip says (its title, or the
/// error that left it unknown). Which state it is in is `(InlineState, <the component's state child>)`, the mark after
/// its label `(InlineMark, <a ui.inline.marks child>)`; neither while nothing is known, and then the chip is drawn in its
/// component's `unknown` state with no mark, never a guessed one. `picture`: a local PNG or JPEG drawn in the icon's place
/// (a link's page's favicon; empty: the state's glyph).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct InlineLive {
    pub key: ScriptString,
    pub tip: ScriptString,
    pub picture: ScriptString,
}

/// Where a live view of web page `url` goes: on a link's hover card (scene/widgets/peek.flecs `hn.ChatPeek`), the seam
/// for an embedded browser. Nothing renders it yet; a browser host (CEF) that renders `url` offscreen hands its frames to
/// the Media cache under `key` (as a Shader's or a Video's are, [`Surface`]) and sets `ready`, and only then does the card
/// add its live view line (`hn.PeekPage`, a `Surface` of `key`), so a card never holds room for a view it lacks. Until
/// one does, the card is the page's OpenGraph data.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct PageView {
    pub url: ScriptString,
    pub key: u64,
    pub ready: bool,
}

/// On a `MarkdownText`'s entity, set by its host: which of its inline components (`tag`, their index in the words) the
/// pointer is over, and whether it is held down. The painter draws that chip's plate in `ui.inline.hover`'s or
/// `ui.inline.pressed`'s Fill and underlines its label; gone when the pointer leaves.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct InlineHot {
    pub tag: u32,
    pub pressed: bool,
}

/// `(InlineState, state)`: the state child of its component a live inline component is drawn as (exclusive).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct InlineState;

/// `(InlineMark, mark)`: the mark drawn after a live inline component's label (exclusive).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct InlineMark;

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

/// What a one-line node (`OneLine`, `Truncate`) shows of `text`: its first line that is not blank, with an ellipsis when
/// more lines follow. A one-line node is one line tall in a box laid for one line (a table row is fixed height), so a hard
/// line break must never reach the shaper: it would measure and draw every line from the node's origin, over its
/// neighbours. Measuring and drawing both go through this, so they agree.
#[must_use]
pub fn one_line(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(['\n', '\r']) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut lines = text.lines().map(str::trim_end).skip_while(|l| l.trim().is_empty());
    let first = lines.next().unwrap_or("");
    if lines.any(|l| !l.trim().is_empty()) {
        std::borrow::Cow::Owned(format!("{first} \u{2026}"))
    } else {
        std::borrow::Cow::Borrowed(first)
    }
}

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

/// The drawing of a `ui.Art`, by name among the host's multi-colour vector drawings (harness-native-render's drawing.rs:
/// the file icons, `rust`, `folder`, ..., which harness-native-ui-inline's files.rs names for each file kind), drawn in
/// its own colours (its dark variant in a dark appearance), fitted and centred in its box, crisp at every zoom. Its node's
/// `Fill` is not read.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Drawing {
    pub name: ScriptString,
}

/// A picture of an `Image`, by handle in the host's media cache.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub handle: ScriptString,
}

/// A live picture drawn from the host's Media by `key` (a Shader's or a Video's element entity id): its latest finished frame,
/// covering the node's box (`cover` 1: cut to the box's shape about its middle; 0: stretched to the box, which layout gives the
/// texture's own shape). Corners from the node's Shape. Nothing is drawn until Media has a frame for it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Surface {
    pub key: u64,
    pub cover: i32,
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

/// A lit paper lantern hanging in the middle of the node (harness-native-render lantern.rs draws it, no file), its
/// light flickering a little and falling on the ground under it: pixel art, as a step's item is. For a pane with
/// nothing in it yet. `on`: whether it shows. `lit`: a stamp (the frame clock's time, say) its owner changes to have it
/// struck again: it goes dark and lights anew, flaring a little past full before it settles.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Lantern {
    pub on: bool,
    pub lit: f32,
}

/// A small pixel-art item, a game's dropped item, that floats and does what it stands for, named by `face` (what a
/// step does: `exec` a terminal with a blinking cursor, `read` a book turning a page, `net` the world turning, `think` a
/// crystal, ...; harness-native-render block.rs draws each, none is a file). When `face` changes the old item sinks away
/// and the new one rises in over `turn` seconds (0: at once). A picture without a file, its texels on whole device pixels, centred in the node's box.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct PixelBlock {
    pub face: ScriptString,
    pub turn: f32,
}

/// blaze's wordmark set on fire (harness-native-blaze), drawn as pixel art over the node's width: its letters start
/// falling in from the top of the view at `start` seconds on the frame clock, the fire catches a beat after the last lands,
/// and from `exit` (when it is after `start`) the letters burn away into embers. A picture without a file. `glow` is how
/// bright the hottest fire burns, in SDR whites: 0 or 1 is no brighter than white; above, the flames, the white-hot
/// letters and the embers rise past it by their heat (an HDR10 video at 203-nit white and a 1000-nit peak sets 4.93).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct BlazeMark {
    pub start: f32,
    pub exit: f32,
    pub glow: f32,
}

/// Kip, den's mascot (harness-native-kip), drawn live: a small voxel creature `size` points tall that plays the moods
/// `mood` names in turn (`"wave"`, `"thinking then celebrate"`; act.rs reads the words, a word it does not know is passed
/// over, none it knows is a wave), the last staying. A node measures his box (`Intrinsic`): room over his head for a hop
/// and under his feet for his shadow, and on his right for a waving arm with his feet at the left (as he stands in a cell's
/// words), or on both sides with his feet in the middle when `centre`. He plays his moods when he first comes into view,
/// or with `settle` stands where they end (a reply read back from history), and plays them again when the pointer comes
/// onto him after they have ended; he looks toward the pointer. Off screen his clock stops. His look and motion are the
/// scene's `ui.kip` (kip.flecs).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Kip {
    pub mood: ScriptString,
    pub size: f32,
    pub centre: bool,
    pub settle: bool,
}

/// Where a `Kip`'s feet stand in its node's box, instead of where its own box puts them: at `x`, `y` (fractions of the
/// box from its top left) moved `dx`, `dy` points. Such a node measures nothing: it is laid on what Kip stands on (an
/// `Overlay` on the composer, his feet on its top edge) and he is drawn past its edges.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct KipStand {
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
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
/// when it last moved). The selection is the scene's one selection plate (`ui.selection`, harness-native-render select.rs),
/// drawn focused or not (a transcript's selection is never focused), pulsing under a `CopyFlash` on the node.
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

/// How a `Vector`'s view box sits in its node's box (less its inset).
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VectorFit {
    /// Uniformly scaled to fit and centred (SVG's `xMidYMid meet`): a drawing.
    #[default]
    Meet,
    /// Each axis scaled to fill (SVG's `none`): a chart's plot, whose strokes stay their width in points.
    Stretch,
}

/// A shape drawn from SVG path data (`d`: M L H V C S Q T A Z, absolute and relative) in its own user units, filled in
/// the node's `Fill` and stroked in its `Stroke` (look api) `stroke_w` wide. The user units go through the transform
/// `ma .. mf` (SVG's `matrix(a b c d e f)`; all four of `ma .. md` at 0 is no transform), then the view box `view_*` is fitted into
/// the node's box less `inset_*` (points) as `fit` says. `stroke_fixed` keeps the stroke `stroke_w` points wide and a
/// `VectorText` `size` points tall whatever the scale (a chart's lines and labels); else both scale with the drawing, as
/// SVG's do. Drawn by the host's path pass: fills
/// with their winding (`even_odd` for the even-odd rule) and an edge one device pixel soft, strokes round at their ends
/// and joins, both cut to the clip the node sits in.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Vector {
    pub d: ScriptString,
    pub view_x: f32,
    pub view_y: f32,
    pub view_w: f32,
    pub view_h: f32,
    pub fit: VectorFit,
    pub ma: f32,
    pub mb: f32,
    pub mc: f32,
    pub md: f32,
    pub me: f32,
    pub mf: f32,
    pub inset_l: f32,
    pub inset_t: f32,
    pub inset_r: f32,
    pub inset_b: f32,
    pub stroke_w: f32,
    pub stroke_fixed: bool,
    pub even_odd: bool,
}

/// Words at a point of a `Vector`'s user space (SVG's `<text>`): `s` set `size` user units tall (bold when said), its
/// baseline at `y`, starting at, centred on or ending at `x` (`anchor` 0, 1, 2), in the node's `Fill`. The point goes
/// through the node's `Vector` (its transform, view box and fit) as a path's would; its `d` may be empty.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct VectorText {
    pub s: ScriptString,
    pub x: f32,
    pub y: f32,
    pub anchor: u8,
    pub size: f32,
    pub bold: bool,
}

hyperion_api_layouts::api_layouts! {
    "Text" => alias Text,
    "MarkdownText" => alias MarkdownText,
    "InlineRef" => alias InlineRef,
    "InlineLive" => alias InlineLive,
    "InlineHot" => alias InlineHot,
    "PageView" => alias PageView,
    "InlineState" => alias InlineState,
    "InlineMark" => alias InlineMark,
    "OneLine" => alias OneLine,
    "Icon" => alias Icon,
    "Mark" => alias Mark,
    "Drawing" => alias Drawing,
    "Picture" => alias Picture,
    "Surface" => alias Surface,
    "Pixelated" => alias Pixelated,
    "Frames" => alias Frames,
    "BlazeMark" => alias BlazeMark,
    "PixelBlock" => alias PixelBlock,
    "Lantern" => alias Lantern,
    "Kip" => alias Kip,
    "KipStand" => alias KipStand,
    "Caret" => alias Caret,
    "TextEdit" => alias TextEdit,
    "Highlight" => alias Highlight,
    "LineNumbers" => alias LineNumbers,
    "TextRange" => alias TextRange,
    "Truncate" => alias Truncate,
    "Vector" => alias Vector,
    "VectorText" => alias VectorText,
    TruncateMode,
    VectorFit,
}

/// Register content components before any scene resolves them.
pub fn register(world: &World) {
    world.component::<Text>().writable();
    world.component::<MarkdownText>().writable();
    world.component::<InlineRef>().writable();
    world.component::<InlineLive>().writable();
    world.component::<InlineHot>().writable();
    world.component::<PageView>().writable();
    world.component::<InlineState>().add_trait::<flecs::Exclusive>();
    world.component::<InlineMark>().add_trait::<flecs::Exclusive>();
    world.component::<OneLine>().writable();
    world.component::<Icon>().writable();
    world.component::<Mark>().writable();
    world.component::<Drawing>().writable();
    world.component::<Picture>().writable();
    world.component::<Surface>().writable();
    world.component::<Pixelated>().writable();
    world.component::<Frames>().writable();
    world.component::<BlazeMark>().writable();
    world.component::<PixelBlock>().writable();
    world.component::<Lantern>().writable();
    world.component::<Kip>().writable();
    world.component::<KipStand>().writable();
    world.component::<Caret>().writable();
    world.component::<TextEdit>().writable();
    world.component::<Highlight>().writable();
    world.component::<LineNumbers>().writable();
    world.component::<TextRange>().writable();
    world.component::<Truncate>().writable();
    world.component::<Vector>().writable();
    world.component::<VectorText>().writable();
}

#[cfg(test)]
mod tests {
    use super::one_line;

    #[test]
    fn one_line_never_breaks() {
        assert_eq!(one_line("plain"), "plain");
        assert_eq!(one_line("395: a\n396: b"), "395: a \u{2026}");
        assert_eq!(one_line("\n\n  first  \n"), "  first");
        assert_eq!(one_line("a\r\nb"), "a \u{2026}");
        assert_eq!(one_line("\n"), "");
    }
}
