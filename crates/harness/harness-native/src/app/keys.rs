//! The keyboard's dispatch: scene/20-keys.flecs and the user's `$HARNESS_HOME/keys.flecs` loaded into the world (and
//! reloaded when the user's file changes), a key matched against their bindings in the active contexts
//! ([`crate::keys::find`]), and the matched action run by the handler registered under its name. The menu bar and the
//! palette's hints are read from the same keymap.
use super::archive::SearchEdit;
use super::*;
use crate::keys::{self, Chord, Keymap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

pub(super) const KEYS: &str = include_str!("../../scene/20-keys.flecs");

/// What a key or a menu item came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No binding took it.
    None,
    /// An action ran.
    Done,
    /// An AppKit action message to send up the responder chain (`copy:`).
    Selector(String),
}

/// An action's handler: the app, the action entity (its data), and the keys that ran it (None from the menu).
type Handler = fn(&mut App, u64, Option<&Chord>);

/// The handlers, by action name (an action without one of its own runs its base's: `SpawnClaude` runs `Spawn`'s).
const HANDLERS: &[(&str, Handler)] = &[
    ("Spawn", |a, act, _| {
        let kind = a
            .world
            .entity_from_id(act)
            .try_cloned::<&keys::SpawnKind>()
            .map(|k| k.kind.as_str().to_owned())
            .unwrap_or_else(|| "shell".into());
        a.spawning(&kind);
        a.spawn(&kind);
    }),
    // cmd+N: a chat of the last-used agent kind, at once.
    ("NewChat", |a, _, _| a.new_chat()),
    // File > New…: the new-object picker, or closed again.
    ("Picker", |a, _, _| {
        if a.picking() {
            a.palette = None;
        } else {
            a.open_picker();
        }
    }),
    ("Archive", |a, _, _| a.kill()),
    // cmd+R: the selected session's name in its row turns into a field (chat/rename.rs).
    ("Rename", |a, _, _| a.rename_start()),
    ("SelectAll", |a, _, _| a.select_all()),
    ("Edit", |a, act, _| {
        let op = a
            .world
            .entity_from_id(act)
            .try_cloned::<&keys::EditOp>()
            .map(|k| k.op.as_str().to_owned())
            .unwrap_or_default();
        a.chat_edit(&op);
    }),
    ("ToggleSidebar", |a, _, _| a.toggle_sidebar()),
    // The browser pane (src/app/browser.rs).
    ("ToggleWeb", |a, _, _| a.toggle_web()),
    ("WebFocusUrl", |a, _, _| a.web_focus_url()),
    ("WebBack", |a, _, _| a.web_nav(harness_proto::browser::Nav::Back)),
    ("WebForward", |a, _, _| a.web_nav(harness_proto::browser::Nav::Forward)),
    ("WebReload", |a, _, _| a.web_nav(harness_proto::browser::Nav::Reload)),
    // cmd+K: the palette, or closed again (over the picker, the palette takes its place).
    ("Palette", |a, _, _| {
        a.palette = if a.palette.as_ref().is_some_and(|p| p.mode == Mode::Commands) {
            None
        } else {
            Some(Palette::new())
        }
    }),
    ("Zoom", |a, act, _| {
        let step =
            a.world.entity_from_id(act).try_cloned::<&keys::ZoomStep>().map_or(1, |z| z.step);
        a.step_zoom(step.signum());
    }),
    ("ZoomReset", |a, _, _| a.step_zoom(0)),
    ("ToggleStats", |a, _, _| a.toggle_stats()),
    ("PrevObject", |a, _, _| {
        if !a.go_back() {
            a.step_object(-1);
        }
    }),
    ("NextObject", |a, _, _| a.step_object(1)),
    ("SelectIndex", |a, act, _| {
        let n = a.world.entity_from_id(act).try_cloned::<&keys::SelectN>().map_or(0, |s| s.n);
        if let Some(e) = usize::try_from(n - 1).ok().and_then(|i| a.objects().get(i).copied()) {
            a.select(Some(e));
        }
    }),
    ("TreeUp", |a, _, _| a.step_object(-1)),
    ("TreeDown", |a, _, _| a.step_object(1)),
    ("TreeFold", |a, _, c| a.tree_fold(c.is_some_and(|c| c.opt))),
    ("TreeUnfold", |a, _, c| a.tree_unfold(c.is_some_and(|c| c.opt))),
    ("TreeLeave", |a, _, _| a.tree_focus = false),
    ("PaletteClose", |a, _, _| a.palette = None),
    ("PaletteUp", |a, _, _| a.palette_step(-1)),
    ("PaletteDown", |a, _, _| a.palette_step(1)),
    ("PaletteRun", |a, _, _| {
        let at = a.palette.as_ref().map_or(0, |p| p.at);
        if let Some((.., d)) = a.entries().into_iter().nth(at) {
            a.choose(d);
        }
    }),
    ("PaletteErase", |a, _, _| {
        let time = a.time;
        if let Some(p) = a.palette.as_mut() {
            p.query.pop();
            p.at = 0;
            p.changed = time;
        }
    }),
    ("SearchClear", |a, _, _| a.archive_search(SearchEdit::Clear)),
    ("SearchErase", |a, _, _| a.archive_search(SearchEdit::Erase)),
    ("ArchiveOpenFirst", |a, _, _| a.archive_open_first()),
    ("SendBytes", |a, act, _| {
        let bytes = a
            .world
            .entity_from_id(act)
            .try_cloned::<&keys::Bytes>()
            .map(|b| keys::caret(b.bytes.as_str()))
            .unwrap_or_default();
        // Typing into the terminal: the tree gives the keyboard back.
        a.tree_focus = false;
        a.typed();
        a.send(&bytes);
    }),
];

/// The user's keys file, watched for changes.
pub(super) struct KeyFile {
    pub path: PathBuf,
    /// Set by the watcher thread when the file's modification time (or its being there) changed.
    pub changed: Arc<AtomicBool>,
}

impl KeyFile {
    /// `$HARNESS_HOME/keys.flecs`, checked once a second on a thread of its own, which wakes the app when it changes.
    pub fn watch(wake: Wake) -> Self {
        let path = crate::link::home().join("keys.flecs");
        let changed = Arc::new(AtomicBool::new(false));
        let (p, flag) = (path.clone(), changed.clone());
        let stamp = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let _ = std::thread::Builder::new().name("keys-watch".into()).spawn(move || {
            let mut last: Option<SystemTime> = stamp(&p);
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = stamp(&p);
                if now != last {
                    last = now;
                    flag.store(true, Ordering::Release);
                    wake();
                }
            }
        });
        Self { path, changed }
    }
}

/// One item of a generated menu.
#[derive(Clone, Debug)]
pub struct MenuLine {
    pub title: String,
    pub action: u64,
    pub selector: Option<String>,
    /// The key equivalent and its modifiers, from the action's binding.
    pub key: Option<(String, KeyMods)>,
    pub sep: bool,
}

impl App {
    /// Run the keyboard's scripts: the scene's, then the user's (a failing user file is reported and left out). What the
    /// last load made is deleted first, so a binding taken out of the user's file goes.
    pub(super) fn load_keys(&mut self) -> Result<(), String> {
        let user = std::fs::read_to_string(&self.key_file.path).ok();
        match self.run_keys(user.as_deref()) {
            Err(e) if user.is_some() => {
                eprintln!(
                    "harness-native: {}: {e}; using the default keys",
                    self.key_file.path.display()
                );
                self.run_keys(None)?;
            }
            r => r?,
        }
        self.keymap = Keymap::read(self.world.world_ref());
        self.keys_gen += 1;
        self.poke();
        Ok(())
    }

    fn run_keys(&mut self, user: Option<&str>) -> Result<(), String> {
        let w = self.world.world_ref();
        for root in keys::ROOTS {
            if let Some(e) = w.try_lookup(root) {
                e.destruct();
            }
        }
        for root in keys::ROOTS {
            w.entity_named(root).add(flecs::OrderedChildren::ID);
        }
        super::load(&self.world, "20-keys", KEYS)?;
        // An empty source still clears what the user's file made last time.
        let e = self.world.script_named("scene::user-keys").build_from_code(user.unwrap_or(""));
        if let Some(err) = script_error(*e) {
            return Err(err);
        }
        Ok(())
    }

    /// The user's keys file changed: load the keys again (the frame calls this).
    pub(super) fn reload_keys_if_changed(&mut self) {
        if self.key_file.changed.swap(false, Ordering::AcqRel) {
            match self.load_keys() {
                Ok(()) => eprintln!(
                    "harness-native: keys reloaded ({} bindings)",
                    self.keymap.binds.len()
                ),
                Err(e) => eprintln!("harness-native: keys: {e}"),
            }
        }
    }

    /// The contexts a key is matched in, innermost first: the palette or the picker while it is open, else the Archive
    /// while it has the keyboard, else the tree (while it has it) over the terminal.
    fn active_contexts(&self) -> Vec<u64> {
        let w = self.world.world_ref();
        let id = |p: &str| w.try_lookup(p).map(|e| *e.id());
        let names: &[&str] = if self.picking() {
            &[keys::PICKER]
        } else if self.palette.is_some() {
            &[keys::PALETTE]
        } else if self.browser_focused() {
            &[keys::BROWSER]
        } else if self.archive_focused() {
            &[keys::ARCHIVE]
        } else if self.renaming() {
            // A name being renamed in its row has the keys: the chat input's edits act on it (chat/rename.rs).
            &[keys::CHAT]
        } else if self.genui_editing() {
            // A cell's field has the keys: the chat input's edits act on it.
            &[keys::CHAT]
        } else if self.chat_focused() {
            if self.tree_focus { &[keys::TREE, keys::CHAT] } else { &[keys::CHAT] }
        } else if self.tree_focus {
            &[keys::TREE, keys::TERMINAL]
        } else {
            &[keys::TERMINAL]
        };
        names.iter().filter_map(|n| id(n)).collect()
    }

    /// Run the binding `chord` matches in the active contexts, if one does.
    pub fn dispatch(&mut self, chord: &Chord) -> Outcome {
        let active = self.active_contexts();
        let Some(action) = keys::find(&self.keymap.binds, chord, &active).map(|b| b.action) else {
            return Outcome::None;
        };
        self.run_action(action, Some(chord))
    }

    /// Run action `id` (a binding's, or a menu item's): its selector for AppKit to send, or its handler.
    pub fn run_action(&mut self, id: u64, chord: Option<&Chord>) -> Outcome {
        self.poke();
        let Some(act) = self.keymap.act(id) else { return Outcome::None };
        if let Some(sel) = &act.selector {
            return Outcome::Selector(sel.clone());
        }
        let handler =
            act.names.iter().find_map(|n| HANDLERS.iter().find(|(h, _)| h == n).map(|(_, f)| *f));
        match handler {
            Some(f) => {
                f(self, id, chord);
                Outcome::Done
            }
            None => {
                eprintln!("harness-native: keys: no handler for act.{}", act.names.join(" / act."));
                Outcome::None
            }
        }
    }

    /// A Command key from the window's key equivalents (or the shot): what came of it.
    pub fn shortcut(&mut self, code: u16, unshifted: Option<char>, m: KeyMods) -> Outcome {
        self.poke();
        // A Command key closes an open row menu (row_menu.rs) and goes on to its binding.
        self.row_menu_close();
        Chord::of(code, unshifted, m).map_or(Outcome::None, |c| self.dispatch(&c))
    }

    /// The menu bar: each menu (in the order the scene first names it) with its actions' items, their key equivalents
    /// from the bindings that run them.
    pub fn menu(&self) -> Vec<(String, Vec<MenuLine>)> {
        let mut bars: Vec<(String, Vec<MenuLine>)> = Vec::new();
        for a in &self.keymap.acts {
            let Some((bar, title, sep)) = &a.menu else { continue };
            let chord = self.keymap.primary(a.id);
            let line = MenuLine {
                title: title.clone(),
                action: a.id,
                selector: a.selector.clone(),
                key: chord.and_then(|c| c.key_equivalent().map(|k| (k, c.mods()))),
                sep: *sep,
            };
            match bars.iter_mut().find(|(b, _)| b == bar) {
                Some((_, items)) => items.push(line),
                None => bars.push((bar.clone(), vec![line])),
            }
        }
        bars
    }

    /// Bumped on every load of the keys (the window rebuilds its menu bar when it changes).
    pub fn keys_gen(&self) -> u64 {
        self.keys_gen
    }

    /// The hint of the binding that runs the action named `name` (`SpawnShell`: `⌘N`), or nothing.
    pub(super) fn hint(&self, name: &str) -> String {
        self.keymap.hint_of(name)
    }

    /// Every key that runs the action named `name`, as one hint (`esc or ⌘.`), or nothing.
    pub(super) fn hints(&self, name: &str) -> String {
        self.keymap.hints_of(name).join(" or ")
    }
}
