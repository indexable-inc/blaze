//! The user's Ghostty config, live (crates/harness/harness-ghostty-config): the terminal's colours, font and padding, the
//! terminal's ground (`tone_term_ground`, under a terminal pane's grid) from the theme's background for each appearance whose
//! theme sets one, and the selection plate in its selection-background. Where nothing is set the scene's tokens and the
//! built-in theme stay. The app's own surfaces (the sidebar, the main card, the cards, menus and inks) are the scene's theme
//! (scene/00-tokens.flecs) and never follow Ghostty: one theme, whatever the terminal is painted in.
use super::*;

/// The terminal's padding when the config sets none (points: left, right, top, bottom).
const DEFAULT_PAD: [f32; 4] = [18.0, 18.0, 16.0, 16.0];

/// The `export mut` tokens (scene/00-tokens.flecs) that follow the Ghostty theme, in [`derived`]'s order: the terminal's
/// ground, alone.
const TOKENS: [&str; 1] = ["tone_term_ground"];

/// What the app takes from the config.
pub(super) struct Look {
    pub live: harness_ghostty_config::Live,
    /// The terminal's theme in the light (0) and dark (1) appearance.
    pub themes: [term::Theme; 2],
    pub size: f32,
    /// Left, right, top, bottom, points.
    pub pad: [f32; 4],
    /// The scene's value of each of [`TOKENS`], read before the first tint.
    scene: Option<Vec<Fill>>,
    /// The scene's selection plate (`ui.selection`'s Fill), read before the first tint.
    selection: Option<Fill>,
}

impl Look {
    /// Read the config, and wake the app whenever it changes after.
    pub fn new(wake: Wake) -> Self {
        harness_ghostty_config::watch(harness_ghostty_config::Sources::standard(), move || wake());
        Self {
            live: harness_ghostty_config::Live::new(),
            themes: [term::Theme::builtin(), term::Theme::builtin()],
            size: TERM_SIZE,
            pad: DEFAULT_PAD,
            scene: None,
            selection: None,
        }
    }
}

fn rgba(v: term::Rgba) -> Rgba {
    Rgba { r: v[0], g: v[1], b: v[2], a: v[3] }
}

/// Each of [`TOKENS`]'s colour from theme `t` in one appearance: the terminal's ground is the theme's background.
fn derived(t: &term::Theme) -> [term::Rgba; 1] {
    [t.bg]
}

/// Token `token`'s value (`export mut`), None when the scene has no such token or it is not a Fill.
fn token(w: WorldRef<'_>, token: &str) -> Option<Fill> {
    let v: serde_json::Value = serde_json::from_str(&w.var_json(token)?).ok()?;
    let c = |k: &str| {
        let c = &v[k];
        let n = |m: &str| c[m].as_f64().map(|x| x as f32);
        Some(Rgba { r: n("r")?, g: n("g")?, b: n("b")?, a: n("a")? })
    };
    Some(Fill { dark: c("dark")?, light: c("light")? })
}

/// Every entity with a Fill, prefabs too, by id.
fn fills(w: WorldRef<'_>) -> Vec<(u64, Fill)> {
    let mut all = Vec::new();
    w.new_query::<&Fill>().each_entity(|e, f| all.push((*e.id(), *f)));
    w.query::<&Fill>().with(flecs::Prefab::ID).build().each_entity(|e, f| all.push((*e.id(), *f)));
    all.sort_unstable_by_key(|&(e, _)| e);
    all.dedup_by_key(|&mut (e, _)| e);
    all
}

/// Token `token` set to `f`. Flecs re-runs the scripts that read it, which sets the Fill of the prefabs that name it; the
/// Fills copied from those when something was made from them (a prefab derived from one, a prefab's children inside another,
/// the instances made so far) do not follow on their own, so each copy that still holds its source's old Fill takes the new
/// one, down every chain of copies. A copy given its own Fill keeps it.
fn retint(w: WorldRef<'_>, token: &str, f: Fill) {
    let json =
        |c: Rgba| format!("{{\"r\": {}, \"g\": {}, \"b\": {}, \"a\": {}}}", c.r, c.g, c.b, c.a);
    let before = fills(w);
    if !w.set_mut_var(token, &format!("{{\"dark\": {}, \"light\": {}}}", json(f.dark), json(f.light))) {
        eprintln!("harness-native: no scene token {token} to tint");
        return;
    }
    let mut work: Vec<(u64, Fill, Fill)> = before
        .iter()
        .filter_map(|&(e, old)| {
            let now = w.entity_from_id(e).try_cloned::<&Fill>()?;
            (now != old).then_some((e, old, now))
        })
        .collect();
    // Each Fill's sources: what the entity is made from (IsA), and, under a parent made from a prefab, that prefab's child of
    // the same name (flecs 4.2 copies a base prefab's children into what is made from it without an IsA to them).
    let mut copies: Vec<(u64, u64)> = Vec::new();
    for &(e, _) in &before {
        let v = w.entity_from_id(e);
        v.each_target(flecs::IsA::ID, |base| copies.push((*base.id(), e)));
        if let Some(parent) = v.parent() {
            let name = v.name();
            parent.each_target(flecs::IsA::ID, |base| {
                if let Some(src) = base.lookup_child(&name) {
                    copies.push((*src.id(), e));
                }
            });
        }
    }
    copies.sort_unstable();
    while let Some((base, old, new)) = work.pop() {
        let from = copies.partition_point(|&(b, _)| b < base);
        for &(_, e) in copies[from..].iter().take_while(|&&(b, _)| b == base) {
            let copy = w.entity_from_id(e);
            if copy.try_cloned::<&Fill>() == Some(old) {
                copy.set(new);
                work.push((e, old, new));
            }
        }
    }
}

impl App {
    /// The config changed (or the app started): take its colours, font and padding.
    pub(super) fn apply_ghostty(&mut self) {
        let cfg = self.look.live.config().clone();
        let blink = cfg.cursor_blink.unwrap_or(false);
        self.look.themes = [
            term::Theme::from_ghostty(&cfg.theme(false), blink),
            term::Theme::from_ghostty(&cfg.theme(true), blink),
        ];
        let size = cfg.font_size.unwrap_or(TERM_SIZE);
        let (px, py) = (cfg.padding_x, cfg.padding_y);
        let pad = [
            px.map_or(DEFAULT_PAD[0], |p| p.0),
            px.map_or(DEFAULT_PAD[1], |p| p.1),
            py.map_or(DEFAULT_PAD[2], |p| p.0),
            py.map_or(DEFAULT_PAD[3], |p| p.1),
        ];
        let families = crate::text::TermFamilies {
            regular: cfg.font_family.clone(),
            bold: cfg.font_family_bold.clone(),
            italic: cfg.font_family_italic.clone(),
            bold_italic: cfg.font_family_bold_italic.clone(),
        };
        let refont = self.paint.get_mut().fonts.set_term_families(families) || size != self.look.size;
        self.look.size = size;
        if pad != self.look.pad {
            self.look.pad = pad;
            self.chrome_laid = None;
        }
        if refont {
            self.grid = Grid::new(&mut self.paint.get_mut().fonts, self.look.size, self.grid.scale);
        }
        let themed = [cfg.light.background.is_some(), cfg.dark.background.is_some()];
        self.tint_tokens(themed);
        self.tint_selection();
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

    /// The terminal's ground from each appearance's theme where it sets a background (`themed`: light, dark), else the
    /// scene's. Code in a chat keeps the app's theme (`code_ink`), as the chat does: only the terminal follows Ghostty.
    fn tint_tokens(&mut self, themed: [bool; 2]) {
        let w = self.world.world_ref();
        let scene = self
            .look
            .scene
            .get_or_insert_with(|| TOKENS.map(|t| token(w, t).unwrap_or_default()).to_vec());
        let [l, d] = &self.look.themes;
        let (dl, dd) = (derived(l), derived(d));
        for (i, name) in TOKENS.iter().enumerate() {
            let s = scene[i];
            let f = Fill {
                light: if themed[0] { rgba(dl[i]) } else { s.light },
                dark: if themed[1] { rgba(dd[i]) } else { s.dark },
            };
            if token(w, name) != Some(f) {
                retint(w, name, f);
            }
        }
    }

    /// The selection plate (`ui.selection`, every surface's) in Ghostty's selection-background where an appearance's theme
    /// sets one, else the scene's.
    fn tint_selection(&mut self) {
        let w = self.world.world_ref();
        let scene = *self.look.selection.get_or_insert_with(|| harness_native_render::select::Look::of(w).fill);
        let e = w.lookup(harness_native_render::select::LOOK);
        let [l, d] = &self.look.themes;
        let f = Fill { light: l.select.map_or(scene.light, rgba), dark: d.select.map_or(scene.dark, rgba) };
        if e.try_cloned::<&Fill>() != Some(f) {
            e.set(f);
        }
    }

    /// Re-read the Ghostty config when one of its files changed (the frame calls this).
    pub(super) fn reload_ghostty_if_changed(&mut self) {
        if self.look.live.poll() {
            self.apply_ghostty();
        }
    }
}
