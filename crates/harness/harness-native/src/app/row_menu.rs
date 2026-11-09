//! A sidebar row's menu: a right press (or a control-click) on a row of the tree or the Archive opens, at the pointer,
//! what can be done to that row's object (scene/widgets/frame.flecs `hn.RowMenu`, a ui.Menu with ui.MenuItem rows): Rename
//! (a session's), Keep in Tree or Don't Keep in Tree, then past a hairline the ones that end something, Close (cmd+W's:
//! killed and put in the Archive) and, while it runs, Kill (stopped, its row left where it is); an archived object's
//! offers Unarchive. Each item shows the keys that do the same from anywhere (scene/20-keys.flecs, or the user's
//! keys.flecs) in its `keys` part.
//!
//! It floats in a window of its own (`Windowed`), its corner on the point the press was (`hn.RowMenuAt`, a placed node
//! of the window's), flipped up or slid by layout to stay on the screen. The pointer and the arrow keys share one
//! highlight (harness_native_ui_view::menu::Nav), Return runs the highlighted item and a press on an item runs it; Escape,
//! a press anywhere else, a Command key or the window losing the keyboard closes it. The press that opens it leaves the
//! selection where it is, and the row it is for shows a ring while it is open (rows.flecs `menu`); only Rename, which
//! works on the selection, selects the row first.
use super::*;
use harness_native_ui_view::menu::Nav;

/// What an item of a row's menu does to the row's object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Rename,
    Pin,
    Unpin,
    Close,
    Kill,
    Unarchive,
}

impl Item {
    const ALL: [Self; 6] = [Self::Rename, Self::Pin, Self::Unpin, Self::Close, Self::Kill, Self::Unarchive];

    /// Its words in the menu.
    fn label(self) -> &'static str {
        match self {
            Self::Rename => "Rename",
            Self::Pin => "Keep in Tree",
            Self::Unpin => "Don't Keep in Tree",
            Self::Close => "Close",
            Self::Kill => "Kill",
            Self::Unarchive => "Unarchive",
        }
    }

    /// Its name in a row's `Action` (`row:<word>:<obj>`).
    fn word(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Close => "close",
            Self::Kill => "kill",
            Self::Unarchive => "unarchive",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.word() == s)
    }

    /// The action (scene/20-keys.flecs) that does the same to the selected object, whose keys the item shows.
    fn act(self) -> Option<&'static str> {
        match self {
            Self::Rename => Some("Rename"),
            Self::Close => Some("Archive"),
            _ => None,
        }
    }
}

/// The open menu: the object it is for, the point it opened at (view points), its highlight, and its rows as last drawn
/// (None for the hairline): what Return runs.
pub struct RowMenu {
    obj: u64,
    at: [f32; 2],
    nav: Nav,
    rows: Vec<Option<Item>>,
}

impl App {
    /// For an offscreen shot's log (shot.rs `HARNESS_NATIVE_SHOT_ROW_MENU`): the object the open menu is for and its items
    /// in order (`|` the hairline), or that none is open.
    pub fn row_menu_line(&self) -> String {
        let Some(m) = &self.row_menu else { return "no menu open".to_owned() };
        let items: Vec<&str> = self.row_menu_rows(m.obj).iter().map(|r| (*r).map_or("|", Item::label)).collect(); // alloc: a shot's log line
        format!("object {}: {}", m.obj, items.join(", "))
    }

    /// Whether a row's menu is open.
    pub(super) fn row_menu_open(&self) -> bool {
        self.row_menu.is_some()
    }

    /// The object whose row's menu is open (its row is ringed).
    pub(super) fn row_menu_for(&self) -> Option<u64> {
        self.row_menu.as_ref().map(|m| m.obj)
    }

    /// A press of button `b` went down at `(x, y)` (window points) with modifiers `m`: a right press or a control-click on
    /// a row opens its menu there (in place of one open on another row); a press off an open menu closes it and goes no
    /// further, as a press off an NSMenu does. Whether the press was the menu's (a press on the open menu is its items').
    pub(super) fn row_menu_down(&mut self, x: f32, y: f32, b: Button, m: KeyMods) -> bool {
        if self.row_menu.is_some() && self.on_row_menu() {
            return false;
        }
        let opens = b == Button::Right || (b == Button::Left && m.ctrl);
        if opens
            && self.palette.is_none()
            && let Some(e) = self.object_at(x, y).filter(|&e| self.is_object(e))
        {
            let z = self.zoom();
            self.chat_pick_close();
            self.row_menu = Some(RowMenu { obj: e, at: [x / z, y / z], nav: Nav::default(), rows: Vec::new() });
            self.poke();
            return true;
        }
        self.row_menu_close()
    }

    /// Whether the pointer is on the open menu's plate (as laid out last frame).
    pub(super) fn on_row_menu(&self) -> bool {
        let at = View::of(self.world.entity_from_id(self.ids.row_menu));
        at.part("plate")
            .and_then(|p| p.entity().try_cloned::<&Laid>())
            .is_some_and(|l| l.contains(self.pointer.x, self.pointer.y))
    }

    /// Close the open menu. Whether one was open.
    pub(super) fn row_menu_close(&mut self) -> bool {
        let open = self.row_menu.take().is_some();
        if open {
            self.poke();
        }
        open
    }

    /// A key while the menu is open: up and down move its highlight, Return (or Enter) runs the highlighted item (or
    /// closes the menu when none is), Escape closes it, and every other key is dropped while it is open, as under an
    /// NSMenu; a Command key closes it and goes on to its binding. Whether the menu took the key.
    pub(super) fn row_menu_key(&mut self, k: &term::KeyIn<'_>) -> bool {
        if self.row_menu.is_none() {
            return false;
        }
        if k.mods.cmd {
            self.row_menu_close();
            return false;
        }
        if k.action == KeyAction::Release {
            return true;
        }
        let named = crate::keys::named(k.code);
        let Some(open) = self.row_menu.as_mut() else { return false };
        match named {
            Some(dir @ ("up" | "down")) => {
                let rows = &open.rows;
                open.nav.step(dir == "down", rows.len(), |i| rows[i].is_some());
                self.poke();
            }
            Some("return" | "enter") => {
                let pick = open.nav.hot.and_then(|i| open.rows.get(i).copied().flatten());
                let obj = open.obj;
                self.row_menu_close();
                if let Some(it) = pick {
                    self.row_menu_run(obj, it);
                }
            }
            Some("escape") => {
                self.row_menu_close();
            }
            _ => {}
        }
        true
    }

    /// A press on an item (`row:<word>:<obj>`): the menu closes and the item runs.
    pub(super) fn row_menu_action(&mut self, rest: &str) {
        let Some((word, obj)) = rest.split_once(':') else { return };
        let (Some(it), Ok(obj)) = (Item::parse(word), obj.parse::<u64>()) else {
            eprintln!("harness-native: row menu: no item {rest:?}");
            return;
        };
        self.row_menu_close();
        self.row_menu_run(obj, it);
    }

    /// Run item `it` on object `obj` (gone by now: nothing).
    fn row_menu_run(&mut self, obj: u64, it: Item) {
        if !self.world.entity_from_id(obj).is_alive() {
            return;
        }
        match it {
            Item::Rename => {
                // cmd+R's rename works on the selection: a rename of another row saves, and this row is selected first.
                self.rename_end(true);
                self.select(Some(obj));
                self.rename_start();
            }
            Item::Pin | Item::Unpin => self.toggle_pin(obj),
            Item::Close => self.close_object(obj),
            Item::Kill => self.kill_object(obj),
            Item::Unarchive => self.unarchive(obj),
        }
        self.poke();
    }

    /// The items object `e`'s menu offers as it is now, a hairline (None) between those that change how it shows and
    /// those that end it.
    fn row_menu_rows(&self, e: u64) -> Vec<Option<Item>> {
        if self.is_archived(e) {
            return vec![Some(Item::Unarchive)]; // alloc: the open menu's one row, once a frame
        }
        let mut rows = Vec::with_capacity(5); // alloc: the open menu's few rows, once a frame
        let o = self.world.entity_from_id(e);
        if self.run_of(e).is_none() && o.try_cloned::<&model::Object>().is_some() {
            rows.push(Some(Item::Rename));
        }
        rows.push(Some(if self.is_pinned(e) { Item::Unpin } else { Item::Pin }));
        rows.push(None);
        rows.push(Some(Item::Close));
        if self.status_of(e) == "running" {
            rows.push(Some(Item::Kill));
        }
        rows
    }

    /// Each frame (sync_view): the open menu on its point, its rows for its object as it is now (an object that went, or
    /// can no longer be acted on, closes it), the highlight kept on its item as rows come and go and moved where the
    /// pointer goes. Closed, the menu leaves on its transition.
    pub(super) fn sync_row_menu(&mut self) {
        if self.row_menu.as_ref().is_some_and(|m| !self.world.entity_from_id(m.obj).is_alive() || !self.is_object(m.obj)) {
            self.row_menu = None;
        }
        let rows = self.row_menu.as_ref().map(|m| self.row_menu_rows(m.obj));
        let hints: Vec<String> = rows // alloc: a hint per row of the open menu, once a frame
            .iter()
            .flatten()
            .map(|it| (*it).and_then(Item::act).map(|a| self.hint(a)).unwrap_or_default())
            .collect();
        let w = self.world.world_ref();
        let at = View::of(w.entity_from_id(self.ids.row_menu));
        if let Some(m) = &self.row_menu {
            let point = Laid { x: m.at[0], y: m.at[1], w: 0.0, h: 0.0 };
            if at.entity().try_cloned::<&Laid>() != Some(point) {
                at.set(point);
            }
        }
        view::for_each_of(at, self.row_menu.as_ref().map(|_| ("plate", "hn.RowMenu", ())), |_, (), _| {});
        let (Some(plate), Some(rows), Some(open)) = (at.part("plate"), rows, self.row_menu.as_mut()) else { return };
        plate.anchored(at.id());
        // The highlight and the pointer's row are kept by the item they are on, not by index: Kill goes as its object
        // stops, and an index would land on what slid into its place.
        let item_at = |i: Option<usize>| i.and_then(|i| open.rows.get(i).copied().flatten());
        let find = |it: Option<Item>| it.and_then(|it| rows.iter().position(|r| *r == Some(it)));
        let (hot, pointer) = (find(item_at(open.nav.hot)), find(item_at(open.nav.pointer_at())));
        open.nav.retarget(hot, pointer);
        open.rows = rows;
        // The row the pointer is on (one of the plate's rows, or inside one), if it is an item.
        let plate_id = plate.id();
        let under = w
            .entity_named(APP)
            .target(Hovered::id(), 0)
            .and_then(|h| std::iter::successors(Some(h), |n| n.parent()).take(8).find(|n| n.parent().is_some_and(|p| p.id() == plate_id)))
            .and_then(|row| row.name().strip_prefix('r').and_then(|i| i.parse::<usize>().ok()))
            .filter(|&i| open.rows.get(i).is_some_and(Option::is_some));
        open.nav.pointer(under);
        let (hot, obj) = (open.nav.hot, open.obj);
        let keyed = open.rows.iter().enumerate().map(|(i, it)| {
            let prefab = if it.is_some() { "ui.MenuItem" } else { "ui.MenuSep" };
            (format!("r{i}"), prefab, (i, *it))
        });
        view::for_each_of(plate, keyed, |row, (i, it), _| {
            let Some(it) = it else {
                view::menu::light(row.entity(), false);
                return;
            };
            view::menu::light(row.entity(), hot == Some(i));
            row.action(&format!("row:{}:{obj}", it.word()));
            if let Some(l) = row.part("label") {
                l.text(it.label());
            }
            if let Some(k) = row.part("keys") {
                k.text(hints.get(i).map_or("", String::as_str));
            }
        });
    }
}
