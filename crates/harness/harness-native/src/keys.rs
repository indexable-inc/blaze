//! The keyboard as data: bindings and actions are entities in scene/20-keys.flecs (and the user's
//! `$HARNESS_HOME/keys.flecs`, run after it). This file holds their components, reads them into a [`Keymap`] and matches a
//! key press against it; src/app/keys.rs runs what matched, and builds the menu bar and the palette's hints from it.
use harness_native_flecs::prelude::*;

/// A binding's keys: `key` a character as typed with no modifiers (`"n"`, `"["`) or a named key (`"left"`,
/// `"backspace"`); each modifier 0 or 1, matched exactly.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Keybind {
    pub key: ScriptString,
    pub cmd: u8,
    pub shift: u8,
    pub opt: u8,
    pub ctrl: u8,
}

/// `(Does, act.X)` on a binding: the action it runs.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Does;

/// `(When, ctx.X)` on a binding: it holds only while that context is active (none: everywhere, below every context).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct When;

/// An `act.Spawn`'s kind of object, and its name in the new-object picker (empty: it is not in the picker).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct SpawnKind {
    pub kind: ScriptString,
    pub title: ScriptString,
}

/// An `act.SelectIndex`'s place in the tree (1 the first).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct SelectN {
    pub n: i32,
}

/// An `act.Zoom`'s step (1 in, -1 out).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ZoomStep {
    pub step: i32,
}

/// An `act.SendBytes`'s bytes, in caret notation (`^[b`: escape, b).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Bytes {
    pub bytes: ScriptString,
}

/// An `act.Edit`'s edit of the chat input (src/app/chat.rs `Edit::of`): `left`, `word_left`, `line_start`, `select_left`,
/// `backspace`, `newline`, `send`, `interrupt`...
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct EditOp {
    pub op: ScriptString,
}

/// An action's place in the menu bar: its menu (`bar`), its title, and a separator above it when `sep` is 1.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MenuItem {
    pub bar: ScriptString,
    pub title: ScriptString,
    pub sep: u8,
}

/// An action that is an AppKit action message (`copy:`), sent up the responder chain rather than run by a handler.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct Selector {
    pub sel: ScriptString,
}

pub fn register(world: &World) {
    world.component::<Keybind>().writable();
    world.component::<Does>().add_trait::<flecs::Exclusive>().writable();
    world.component::<When>().writable();
    world.component::<SpawnKind>().writable();
    world.component::<SelectN>().writable();
    world.component::<ZoomStep>().writable();
    world.component::<Bytes>().writable();
    world.component::<EditOp>().writable();
    world.component::<MenuItem>().writable();
    world.component::<Selector>().writable();
}

/// The parents the scene's keyboard entities live under (made `OrderedChildren`, so later means declared later).
pub const ROOTS: [&str; 3] = ["ctx", "act", "key"];

/// The contexts, by their path under `ctx`.
pub const TERMINAL: &str = "ctx::Terminal";
pub const TREE: &str = "ctx::Tree";
pub const PALETTE: &str = "ctx::Palette";
pub const PICKER: &str = "ctx::Picker";
pub const ARCHIVE: &str = "ctx::Archive";
pub const CHAT: &str = "ctx::Chat";
pub const BROWSER: &str = "ctx::Browser";

/// Keys pressed together, as a binding names them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Chord {
    pub key: String,
    pub cmd: bool,
    pub shift: bool,
    pub opt: bool,
    pub ctrl: bool,
}

/// The macOS virtual key codes of the keys a binding names by word.
const NAMED: [(&str, u16); 27] = [
    ("return", 36),
    ("enter", 76),
    ("tab", 48),
    ("space", 49),
    ("backspace", 51),
    ("escape", 53),
    ("delete", 117),
    ("home", 115),
    ("end", 119),
    ("pageup", 116),
    ("pagedown", 121),
    ("left", 123),
    ("right", 124),
    ("down", 125),
    ("up", 126),
    ("f1", 122),
    ("f2", 120),
    ("f3", 99),
    ("f4", 118),
    ("f5", 96),
    ("f6", 97),
    ("f7", 98),
    ("f8", 100),
    ("f9", 101),
    ("f10", 109),
    ("f11", 103),
    ("f12", 111),
];

/// The word for key `code`, when a binding names it by word.
#[must_use]
pub fn named(code: u16) -> Option<&'static str> {
    NAMED.iter().find(|(_, c)| *c == code).map(|(n, _)| *n)
}

/// The key code of a named key.
#[must_use]
pub fn code_of(name: &str) -> Option<u16> {
    NAMED.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

impl Chord {
    /// A key press: its code, its character with no modifiers, and the modifiers held.
    #[must_use]
    pub fn of(code: u16, unshifted: Option<char>, m: crate::term::Mods) -> Option<Self> {
        let key = match named(code) {
            Some(n) => n.to_owned(),
            None => unshifted
                .filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))?
                .to_lowercase()
                .collect(),
        };
        Some(Self { key, cmd: m.cmd, shift: m.shift, opt: m.alt, ctrl: m.ctrl })
    }

    /// `cmd+shift+t`, `left`, `opt+backspace`: modifiers then the key, joined by `+` (a bare key is no modifiers).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts: Vec<&str> = s.split('+').collect();
        // `cmd++`: the key is the plus.
        if s.ends_with("++") {
            parts.truncate(parts.len().saturating_sub(2));
            parts.push("+");
        }
        let key = parts.pop().filter(|k| !k.is_empty())?.to_lowercase();
        let mut c = Self { key, ..Self::default() };
        for m in parts {
            match m {
                "cmd" => c.cmd = true,
                "shift" => c.shift = true,
                "opt" | "alt" => c.opt = true,
                "ctrl" => c.ctrl = true,
                _ => return None,
            }
        }
        Some(c)
    }

    fn of_bind(k: &Keybind) -> Self {
        Self {
            key: k.key.as_str().to_lowercase(),
            cmd: k.cmd != 0,
            shift: k.shift != 0,
            opt: k.opt != 0,
            ctrl: k.ctrl != 0,
        }
    }

    /// The modifiers as `term::Mods`.
    #[must_use]
    pub fn mods(&self) -> crate::term::Mods {
        crate::term::Mods { cmd: self.cmd, shift: self.shift, ctrl: self.ctrl, alt: self.opt }
    }

    /// As the menu bar and the palette show it: `⌘⇧T`, `⌥←`.
    #[must_use]
    pub fn hint(&self) -> String {
        let mut s = String::new();
        for (on, g) in [
            (self.ctrl, '\u{2303}'),
            (self.opt, '\u{2325}'),
            (self.shift, '\u{21E7}'),
            (self.cmd, '\u{2318}'),
        ] {
            if on {
                s.push(g);
            }
        }
        s.push_str(match self.key.as_str() {
            "left" => "\u{2190}",
            "right" => "\u{2192}",
            "up" => "\u{2191}",
            "down" => "\u{2193}",
            "backspace" => "\u{232B}",
            "delete" => "\u{2326}",
            "return" => "\u{21A9}",
            "enter" => "\u{2324}",
            "escape" => "\u{238B}",
            "tab" => "\u{21E5}",
            "space" => "Space",
            "-" => "\u{2212}",
            k => return s + &k.to_uppercase(),
        });
        s
    }

    /// The `NSMenuItem` key equivalent for this key (without its modifiers), when it can have one.
    #[must_use]
    pub fn key_equivalent(&self) -> Option<String> {
        let s = match self.key.as_str() {
            "left" => "\u{F702}",
            "right" => "\u{F703}",
            "up" => "\u{F700}",
            "down" => "\u{F701}",
            "backspace" => "\u{8}",
            "delete" => "\u{F728}",
            "return" => "\r",
            "enter" => "\u{3}",
            "escape" => "\u{1b}",
            "tab" => "\t",
            "space" => " ",
            k if k.chars().count() == 1 => k,
            _ => return None,
        };
        Some(s.to_owned())
    }
}

/// Caret notation to bytes: `^[` escape, `^?` DEL, `^@`..`^_` the control characters; anything else as is.
#[must_use]
pub fn caret(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'^' && i + 1 < b.len() {
            let c = b[i + 1].to_ascii_uppercase();
            match c {
                b'?' => out.push(0x7f),
                b'@'..=b'_' => out.push(c - b'@'),
                _ => {
                    out.push(b'^');
                    i += 1;
                    continue;
                }
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// One binding as read from the world: its keys, the contexts it holds in (empty: everywhere), its action and its place
/// in load order (later wins a tie).
#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub name: String,
    pub chord: Chord,
    pub ctxs: Vec<u64>,
    pub action: u64,
    pub seq: usize,
}

/// The binding `chord` runs while the contexts `active` hold (innermost first): the one whose context comes first in
/// `active` (a binding with no context ranks after them all), then the latest.
#[must_use]
pub fn find<'b>(binds: &'b [Bind], chord: &Chord, active: &[u64]) -> Option<&'b Bind> {
    let rank = |b: &Bind| -> Option<usize> {
        if b.ctxs.is_empty() {
            return Some(active.len());
        }
        b.ctxs.iter().filter_map(|c| active.iter().position(|a| a == c)).min()
    };
    binds
        .iter()
        .filter(|b| &b.chord == chord)
        .filter_map(|b| rank(b).map(|r| (r, b)))
        .min_by(|(ra, a), (rb, b)| ra.cmp(rb).then(b.seq.cmp(&a.seq)))
        .map(|(_, b)| b)
}

/// Whether binding `b` is shadowed: a later one has the same keys in the same contexts.
#[must_use]
pub fn shadowed(binds: &[Bind], b: &Bind) -> bool {
    binds.iter().any(|o| o.seq > b.seq && o.chord == b.chord && o.ctxs == b.ctxs)
}

/// One action as read from the world.
#[derive(Clone, Debug, Default)]
pub struct Act {
    pub id: u64,
    /// Its name, then its base's (`SpawnClaude`, `Spawn`): the first with a handler runs it.
    pub names: Vec<String>,
    pub menu: Option<(String, String, bool)>,
    pub selector: Option<String>,
}

/// Every binding and action the scene declares, in load order.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    pub binds: Vec<Bind>,
    pub acts: Vec<Act>,
}

impl Keymap {
    /// Read the world's `key` and `act` entities (each parent's children in the order declared).
    #[must_use]
    pub fn read(w: WorldRef<'_>) -> Self {
        let mut m = Self::default();
        let mut problems = Vec::new();
        if let Some(acts) = w.try_lookup("act") {
            acts.each_child(|a| {
                if a.has(flecs::Prefab::ID) {
                    return;
                }
                let mut names = vec![a.name()];
                if let Some(base) = a.target(flecs::IsA::ID, 0) {
                    names.push(base.name());
                }
                let menu = a.try_cloned::<&MenuItem>().map(|mi| {
                    (mi.bar.as_str().to_owned(), mi.title.as_str().to_owned(), mi.sep != 0)
                });
                let selector = a.try_cloned::<&Selector>().map(|s| s.sel.as_str().to_owned());
                m.acts.push(Act { id: *a.id(), names, menu, selector });
            });
        }
        let act_root = w.try_lookup("act").map(|a| *a.id());
        if let Some(keys) = w.try_lookup("key") {
            keys.each_child(|k| {
                let Some(kb) = k.try_cloned::<&Keybind>() else { return };
                let name = k.name();
                let Some(action) = k.target(Does::id(), 0) else {
                    problems.push(format!("key.{name} does nothing (no (Does, act.X))"));
                    return;
                };
                if action.parent().map(|p| *p.id()) != act_root {
                    problems
                        .push(format!("key.{name} does {}, which is not an act.X", action.name()));
                    return;
                }
                let mut ctxs = Vec::new();
                k.each_target(When::id(), |c| ctxs.push(*c.id()));
                ctxs.sort_unstable();
                let seq = m.binds.len();
                m.binds.push(Bind {
                    name,
                    chord: Chord::of_bind(&kb),
                    ctxs,
                    action: *action.id(),
                    seq,
                });
            });
        }
        for p in problems {
            eprintln!("harness-native: keys: {p}");
        }
        m
    }

    #[must_use]
    pub fn act(&self, id: u64) -> Option<&Act> {
        self.acts.iter().find(|a| a.id == id)
    }

    /// The action named `name` (`SpawnShell`).
    #[must_use]
    pub fn act_named(&self, name: &str) -> Option<&Act> {
        self.acts.iter().find(|a| a.names.first().is_some_and(|n| n == name))
    }

    /// The keys that run action `id` everywhere (the first binding of it with no context that nothing shadows): what the
    /// menu bar and the palette show for it.
    #[must_use]
    pub fn primary(&self, id: u64) -> Option<&Chord> {
        self.binds
            .iter()
            .find(|b| b.action == id && b.ctxs.is_empty() && !shadowed(&self.binds, b))
            .map(|b| &b.chord)
    }

    /// The keys that run action `id` in context `ctx` alone (its first binding there that nothing shadows): the key a
    /// picker row shows in its keycap.
    #[must_use]
    pub fn in_context(&self, id: u64, ctx: u64) -> Option<&Chord> {
        self.binds
            .iter()
            .find(|b| b.action == id && b.ctxs == [ctx] && !shadowed(&self.binds, b))
            .map(|b| &b.chord)
    }

    /// Every binding of the action named `name` that nothing shadows, in any context, as hints in the order the keys file
    /// binds them (`⎋`, `⌘.`); a control chord is left out (a terminal's habit, kept working but not said).
    #[must_use]
    pub fn hints_of(&self, name: &str) -> Vec<String> {
        let Some(a) = self.act_named(name) else { return Vec::new() };
        let mut out: Vec<String> = Vec::new();
        for b in self.binds.iter().filter(|b| b.action == a.id && !b.chord.ctrl && !shadowed(&self.binds, b)) {
            let h = b.chord.hint();
            if !out.contains(&h) {
                out.push(h);
            }
        }
        out
    }

    /// [`primary`](Self::primary)'s hint for the action named `name`, or nothing.
    #[must_use]
    pub fn hint_of(&self, name: &str) -> String {
        self.act_named(name).and_then(|a| self.primary(a.id)).map(Chord::hint).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(seq: usize, chord: &str, ctxs: &[u64], action: u64) -> Bind {
        Bind {
            name: format!("b{seq}"),
            chord: Chord::parse(chord).unwrap(),
            ctxs: ctxs.to_vec(),
            action,
            seq,
        }
    }

    #[test]
    fn matcher_context_precedence_and_override_order() {
        const TREE: u64 = 10;
        const TERM: u64 = 11;
        const PAL: u64 = 12;
        let binds = vec![
            b(0, "cmd+n", &[], 1),
            b(1, "up", &[TREE], 2),
            b(2, "opt+left", &[TERM], 3),
            b(3, "opt+left", &[TREE], 4),
            b(4, "escape", &[PAL], 5),
            // A user override after the defaults: same keys, same (no) context; it wins.
            b(5, "cmd+n", &[], 6),
        ];
        let at = |c: &str, active: &[u64]| {
            find(&binds, &Chord::parse(c).unwrap(), active).map(|b| b.action)
        };
        // Later wins a tie.
        assert_eq!(at("cmd+n", &[TERM]), Some(6));
        assert!(shadowed(&binds, &binds[0]));
        assert!(!shadowed(&binds, &binds[5]));
        // A context binding holds only while its context is active.
        assert_eq!(at("up", &[TERM]), None);
        assert_eq!(at("up", &[TREE, TERM]), Some(2));
        // The innermost active context wins over an outer one, whatever their order in the file.
        assert_eq!(at("opt+left", &[TREE, TERM]), Some(4));
        assert_eq!(at("opt+left", &[TERM]), Some(3));
        assert_eq!(at("escape", &[PAL]), Some(5));
        assert_eq!(at("escape", &[TREE, TERM]), None);
        // Modifiers match exactly.
        assert_eq!(at("cmd+shift+n", &[TERM]), None);
        assert_eq!(Chord::parse("cmd+shift+t").unwrap().hint(), "\u{21E7}\u{2318}T");
        assert_eq!(caret("^[b"), b"\x1bb");
        assert_eq!(caret("^[^?"), b"\x1b\x7f");
        assert_eq!(caret("^_"), b"\x1f");
    }
}
