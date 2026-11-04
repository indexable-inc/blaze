//! The user's Ghostty config, live (crates/harness/harness-ghostty-config): the terminal's colours, font and padding, and the
//! tokens that sit on the terminal's ground (the main card, the title bar's inks, the sidebar's tint, the plates),
//! derived from the theme's ground and ink for each appearance whose theme sets a background. Where nothing is set the
//! scene's tokens and the built-in theme stay.
use super::*;

/// The terminal's padding when the config sets none (points: left, right, top, bottom).
const DEFAULT_PAD: [f32; 4] = [18.0, 18.0, 16.0, 16.0];

/// The prefabs whose Fill follows the theme, each with the scene's own Fill (kept for an appearance with no theme).
const TINTED: [&str; 7] = ["Plate", "hn.Main", "hn.Palette.card", "hn.SideClip", "hn.TitleBar.title", "hn.TitleBar.back", "hn.TitleBar.sub"];

/// What the app takes from the config.
pub(super) struct Look {
    pub live: harness_ghostty_config::Live,
    /// The terminal's theme in the light (0) and dark (1) appearance.
    pub themes: [term::Theme; 2],
    pub size: f32,
    /// Left, right, top, bottom, points.
    pub pad: [f32; 4],
    /// The scene's Fill of each of [`TINTED`], read before the first tint.
    scene: Option<Vec<Fill>>,
}

impl Look {
    /// Read the config, and wake the app whenever it changes after.
    pub fn new(wake: Wake) -> Self {
        harness_ghostty_config::watch(harness_ghostty_config::Sources::standard(), move || wake());
        Self { live: harness_ghostty_config::Live::new(), themes: [term::Theme::builtin(), term::Theme::builtin()], size: TERM_SIZE, pad: DEFAULT_PAD, scene: None }
    }
}

fn rgba(v: term::Rgba) -> Rgba {
    Rgba { r: v[0], g: v[1], b: v[2], a: v[3] }
}

fn alpha(mut c: term::Rgba, a: f32) -> term::Rgba {
    c[3] = a;
    c
}

fn mix(a: term::Rgba, b: term::Rgba, t: f32) -> term::Rgba {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 1.0]
}

/// Each of [`TINTED`]'s colour from theme `t` in one appearance (`dark` picks the light or dark variant of the rule).
fn derived(t: &term::Theme, dark: bool) -> [term::Rgba; 7] {
    let ground = t.bg;
    // The palette's card a touch off the ground: toward the ink in the dark, toward white in the light.
    let card = if dark { mix(t.bg, t.fg, 0.04) } else { mix(t.bg, [1.0; 4], 0.6) };
    let side = t.bg;
    let ink = alpha(t.fg, 0.92);
    let muted = alpha(t.fg, if dark { 0.5 } else { 0.55 });
    [ground, ground, card, side, ink, muted, muted]
}

/// `f` on prefab `path` (what instances made from now on copy) and on every instance made from it so far.
fn retint(w: WorldRef<'_>, path: &str, f: Fill) {
    let Some(pf) = view::prefab(w, path) else { return };
    let id = *pf.id();
    pf.set(f);
    let mut made = Vec::new();
    w.new_query::<&Fill>().each_entity(|e, _| {
        if e.owns((flecs::IsA::ID, id)) {
            made.push(*e.id());
        }
    });
    for e in made {
        w.entity_from_id(e).set(f);
    }
}

impl App {
    /// The config changed (or the app started): take its colours, font and padding.
    pub(super) fn apply_ghostty(&mut self) {
        let cfg = self.look.live.config().clone();
        let blink = cfg.cursor_blink.unwrap_or(false);
        self.look.themes = [term::Theme::from_ghostty(&cfg.theme(false), blink), term::Theme::from_ghostty(&cfg.theme(true), blink)];
        let size = cfg.font_size.unwrap_or(TERM_SIZE);
        let (px, py) = (cfg.padding_x, cfg.padding_y);
        let pad = [px.map_or(DEFAULT_PAD[0], |p| p.0), px.map_or(DEFAULT_PAD[1], |p| p.1), py.map_or(DEFAULT_PAD[2], |p| p.0), py.map_or(DEFAULT_PAD[3], |p| p.1)];
        let families = crate::text::TermFamilies { regular: cfg.font_family.clone(), bold: cfg.font_family_bold.clone(), italic: cfg.font_family_italic.clone(), bold_italic: cfg.font_family_bold_italic.clone() };
        let refont = self.fonts.set_term_families(families) || size != self.look.size;
        self.look.size = size;
        if pad != self.look.pad {
            self.look.pad = pad;
            self.chrome_laid = None;
        }
        if refont {
            self.grid = Grid::new(&mut self.fonts, self.look.size, self.grid.scale);
        }
        let themed = [cfg.light.background.is_some(), cfg.dark.background.is_some()];
        self.tint_tokens(themed);
        eprintln!(
            "harness-native: ghostty config {} (themed light {} dark {}; font {:?} {size}, cell {}x{} px @{}x, padding {pad:?}, blink {blink})",
            if cfg.found { "read" } else { "not found" },
            themed[0],
            themed[1],
            cfg.font_family.first(),
            self.grid.cwp(),
            self.grid.chp(),
            self.grid.scale,
        );
        self.poke();
    }

    /// The tokens on the terminal's ground from each appearance's theme where it sets a background (`themed`: light,
    /// dark), else the scene's: the main card and the plates in its ground, the title bar's ink and muted ink in its
    /// foreground, the sidebar's tint its ground part see-through.
    fn tint_tokens(&mut self, themed: [bool; 2]) {
        let w = self.world.world_ref();
        let scene = self.look.scene.get_or_insert_with(|| TINTED.map(|p| view::prefab(w, p).and_then(|e| e.try_cloned::<&Fill>()).unwrap_or_default()).to_vec());
        let [l, d] = &self.look.themes;
        let (dl, dd) = (derived(l, false), derived(d, true));
        for (i, path) in TINTED.iter().enumerate() {
            let s = scene[i];
            let f = Fill { light: if themed[0] { rgba(dl[i]) } else { s.light }, dark: if themed[1] { rgba(dd[i]) } else { s.dark } };
            if view::prefab(w, path).and_then(|e| e.try_cloned::<&Fill>()) != Some(f) {
                retint(w, path, f);
            }
        }
    }

    /// Re-read the Ghostty config when one of its files changed (the frame calls this).
    pub(super) fn reload_ghostty_if_changed(&mut self) {
        if self.look.live.poll() {
            self.apply_ghostty();
        }
    }
}
