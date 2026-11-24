//! Shaped front-page [`PageModel`] parsed zero-dependency via `blaze_json::Reader` and mounted
//! directly into a `blaze_ecs::World` (`ChildOf`, `OrderedChildren`, `DontFragment` tags).

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, Entity, World};
use blaze_json::{Event, Limits, Reader, Str};
use blaze_own::{String as OwnString, Vec as OwnVec};
use blaze_ui::ChildOrder;

use crate::error::SiteError;
use crate::palette::{ImageName, RoleName};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipKind {
    Plain,
    Cmd,
    Path,
    File,
    Badge,
}

impl ChipKind {
    fn from_str(s: &str) -> Self {
        match s {
            "cmd" => Self::Cmd,
            "path" => Self::Path,
            "file" => Self::File,
            "badge" => Self::Badge,
            _ => Self::Plain,
        }
    }
}

/// One unbreakable shaped word or inline picture in em units.
pub struct Word<A: Allocator + Copy> {
    pub text: OwnString<A>,
    pub role: RoleName,
    /// Per code-point glyph index in `SlugFontTable` (`-1` = draws nothing).
    pub glyphs: OwnVec<i32, A>,
    /// Per code-point x origin in em of role size (HarfBuzz-kerned).
    pub xs: OwnVec<f32, A>,
    pub adv: f32,
    pub space: f32,
    pub link: Option<u32>,
    pub chip: Option<u32>,
    pub chip_kind: Option<ChipKind>,
    pub deco: bool,
    pub image: Option<ImageName>,
    pub rise: f32,
    pub drop: f32,
    pub raise: f32,
    pub part: Option<u32>,
    pub scramble: OwnVec<u32, A>,
}

impl<A: Allocator + Copy> Word<A> {
    pub fn empty_in(role: RoleName, alloc: A) -> Self {
        Self {
            text: OwnString::new_in(alloc),
            role,
            glyphs: OwnVec::new_in(alloc),
            xs: OwnVec::new_in(alloc),
            adv: 0.0,
            space: 0.0,
            link: None,
            chip: None,
            chip_kind: None,
            deco: false,
            image: None,
            rise: 0.0,
            drop: 0.0,
            raise: 0.0,
            part: None,
            scramble: OwnVec::new_in(alloc),
        }
    }
}

pub struct Link<A: Allocator + Copy> {
    pub href: OwnString<A>,
    pub tip: Option<OwnString<A>>,
    pub note: Option<OwnString<A>>,
}

pub struct RaceRow<A: Allocator + Copy> {
    pub who: OwnVec<Word<A>, A>,
    pub took: OwnVec<Word<A>, A>,
    pub share: f32,
    pub lead: bool,
}

pub struct Race<A: Allocator + Copy> {
    pub rows: OwnVec<RaceRow<A>, A>,
    pub title: OwnString<A>,
}

/// Flat arena-indexed list item (zero recursion on arbitrary nesting depth!).
pub struct ItemNode<A: Allocator + Copy> {
    pub words: OwnVec<Word<A>, A>,
    /// Child indices into `PageModel::items_arena`.
    pub children: OwnVec<usize, A>,
    pub race: Option<Race<A>>,
}

pub struct NoteEntry<A: Allocator + Copy> {
    pub id: OwnString<A>,
    pub words: OwnVec<Word<A>, A>,
}

pub enum Block<A: Allocator + Copy> {
    Bar {
        name: OwnVec<Word<A>, A>,
        links: OwnVec<Word<A>, A>,
        backed: OwnVec<Word<A>, A>,
    },
    Title {
        words: OwnVec<Word<A>, A>,
    },
    Label {
        words: OwnVec<Word<A>, A>,
    },
    Command {
        words: OwnVec<Word<A>, A>,
        parts: OwnVec<OwnString<A>, A>,
        copy: OwnString<A>,
        button: OwnVec<Word<A>, A>,
        copied: OwnVec<Word<A>, A>,
    },
    List {
        /// Root item indices into `PageModel::items_arena`.
        items: OwnVec<usize, A>,
    },
    Soon {
        label: OwnVec<Word<A>, A>,
        items: OwnVec<usize, A>,
    },
    Sign {
        image: ImageName,
        closing: OwnVec<Word<A>, A>,
        words: OwnVec<Word<A>, A>,
        contact: OwnVec<Word<A>, A>,
    },
    Notes {
        notes: OwnVec<NoteEntry<A>, A>,
    },
    Footer {
        links: OwnVec<Word<A>, A>,
    },
}

pub struct TipRow<A: Allocator + Copy> {
    pub says: OwnVec<Word<A>, A>,
    pub does: OwnVec<Word<A>, A>,
}

pub enum TipKind<A: Allocator + Copy> {
    Text {
        title: Option<OwnVec<Word<A>, A>>,
        body: OwnVec<Word<A>, A>,
    },
    Rows {
        rows: OwnVec<TipRow<A>, A>,
    },
}

pub struct TipEntry<A: Allocator + Copy> {
    pub key: OwnString<A>,
    pub kind: TipKind<A>,
}

/// Complete front-page semantic model parsed from `page.<hash>.json`.
pub struct PageModel<A: Allocator + Copy> {
    pub table: OwnString<A>,
    pub blocks: OwnVec<Block<A>, A>,
    pub items_arena: OwnVec<ItemNode<A>, A>,
    pub links: OwnVec<Link<A>, A>,
    pub tips: OwnVec<TipEntry<A>, A>,
    pub scramble_note: OwnVec<u32, A>,
    pub scramble_tip: OwnVec<u32, A>,
}

/// ECS component identifying a semantic page node mounted inside `blaze_ecs::World`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteNodeKind {
    PageRoot,
    Block(u32),
    ListItem(u32),
    Link(u32),
    CommandPart(u32),
    CopyButton,
    FindBar,
}

/// Flecs 4.2 `DontFragment` tag for hovered site entities (zero archetype table migrations!).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteHovered;

/// Flecs 4.2 `DontFragment` tag for focused site entities (zero archetype table migrations!).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteFocused;

/// Flecs 4.2 `DontFragment` tag for pressed site entities (zero archetype table migrations!).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SitePressed;

/// Owned HarfBuzz-shaped word stream stored directly as a `blaze_ecs` component so every piece of
/// text, glyph run, role, and inline icon lives inside the ECS (no external `PageModel` lookup!).
pub struct EcsWords<A: Allocator + Copy>(pub OwnVec<Word<A>, A>);

/// Owned clipboard payload component attached to command / copy entities in `blaze_ecs::World`.
pub struct SiteCopyText<A: Allocator + Copy>(pub OwnString<A>);

/// Flecs-style singleton metadata stored directly in `blaze_ecs::World` (`world.singleton::<SiteMeta<A>>()`).
pub struct SiteMeta<A: Allocator + Copy> {
    pub domain: OwnString<A>,
}

/// Semantic ECS marker tags identifying page roles for declarative `ecs_query!` access.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteWordmark;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteBacked;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteHeroTitle;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteInstallCmd;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteCopyLabel;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteFeatureList;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteFeatureItem;

fn copy_word_in<A: Allocator + Copy>(w: &Word<A>, alloc: A) -> Result<Word<A>, SiteError> {
    let text = OwnString::try_from_str_in(w.text.as_str(), alloc)?;
    let mut glyphs = OwnVec::try_with_capacity_in(w.glyphs.len(), alloc)?;
    for &g in w.glyphs.as_slice() {
        glyphs.try_push(g).map_err(|r| SiteError::Alloc(r.why))?;
    }
    let mut xs = OwnVec::try_with_capacity_in(w.xs.len(), alloc)?;
    for &x in w.xs.as_slice() {
        xs.try_push(x).map_err(|r| SiteError::Alloc(r.why))?;
    }
    let mut scramble = OwnVec::try_with_capacity_in(w.scramble.len(), alloc)?;
    for &s in w.scramble.as_slice() {
        scramble.try_push(s).map_err(|r| SiteError::Alloc(r.why))?;
    }
    Ok(Word {
        text,
        role: w.role,
        glyphs,
        xs,
        adv: w.adv,
        space: w.space,
        link: w.link,
        chip: w.chip,
        chip_kind: w.chip_kind,
        deco: w.deco,
        image: w.image,
        rise: w.rise,
        drop: w.drop,
        raise: w.raise,
        part: w.part,
        scramble,
    })
}

fn copy_words_in<A: Allocator + Copy>(
    words: &[Word<A>],
    alloc: A,
) -> Result<EcsWords<A>, SiteError> {
    let mut out = OwnVec::try_with_capacity_in(words.len(), alloc)?;
    for w in words {
        out.try_push(copy_word_in(w, alloc)?).map_err(|r| SiteError::Alloc(r.why))?;
    }
    Ok(EcsWords(out))
}

fn extract_domain_from_copy(copy_cmd: &str) -> &str {
    let Some(pos) = copy_cmd.find("https://") else {
        return "";
    };
    let rest = copy_cmd.get(pos + 8..).unwrap_or("");
    let end = rest.find(|c: char| c.is_ascii_whitespace() || c == '|').unwrap_or(rest.len());
    rest.get(..end).unwrap_or("")
}

/// Entities created when mounting [`PageModel`] into a `blaze_ecs::World`.
pub struct SiteEcsHandles<A: Allocator + Copy> {
    pub root: Entity,
    pub copy_button: Entity,
    pub find_bar: Entity,
    pub link_entities: OwnVec<Entity, A>,
    pub part_entities: OwnVec<Entity, A>,
}

impl<A: Allocator + Copy + 'static> PageModel<A> {
    pub fn empty_in(alloc: A) -> Self {
        Self {
            table: OwnString::new_in(alloc),
            blocks: OwnVec::new_in(alloc),
            items_arena: OwnVec::new_in(alloc),
            links: OwnVec::new_in(alloc),
            tips: OwnVec::new_in(alloc),
            scramble_note: OwnVec::new_in(alloc),
            scramble_tip: OwnVec::new_in(alloc),
        }
    }

    pub fn find_tip(&self, key: &str) -> Option<&TipEntry<A>> {
        for t in self.tips.as_slice() {
            if t.key.as_str() == key {
                return Some(t);
            }
        }
        None
    }

    pub fn scramble_pool(&self, role: RoleName) -> &[u32] {
        match role {
            RoleName::Note => self.scramble_note.as_slice(),
            RoleName::Tip => self.scramble_tip.as_slice(),
            _ => &[],
        }
    }

    /// Mount this page's blocks, items, links, command parts, copy button, and find bar into a
    /// relational `blaze_ecs::World` (`ChildOf` + `OrderedChildren` + `EcsWords<A>` +
    /// Flecs 4.2 `DontFragment` interaction tags + `SiteMeta<A>` singleton).
    pub fn mount_into_ecs(&self, world: &mut World<A>, alloc: A) -> Result<SiteEcsHandles<A>, SiteError> {
        let _ = world.register_copy::<SiteNodeKind>()?;
        let _ = world.register_copy::<ChildOrder>()?;
        world.mark_tag_dont_fragment::<SiteHovered>()?;
        world.mark_tag_dont_fragment::<SiteFocused>()?;
        world.mark_tag_dont_fragment::<SitePressed>()?;

        let root = world.spawn_named("BlazeSite::Root")?;
        world.set_copy(root, SiteNodeKind::PageRoot)?;
        let ordered_tag = world.ordered_children();
        world.add_tag(root, ordered_tag)?;

        let copy_button = world.spawn_named("BlazeSite::CopyButton")?;
        world.set_copy(copy_button, SiteNodeKind::CopyButton)?;
        world.relate::<ChildOf>(copy_button, root)?;

        let mut domain_str: Option<&str> = None;
        let mut first_list_tagged = false;
        let mut first_cmd_tagged = false;

        for (bi, block) in self.blocks.as_slice().iter().enumerate() {
            let b_ent = world.spawn()?;
            world.set_copy(b_ent, SiteNodeKind::Block(u32::try_from(bi).unwrap_or(0)))?;
            world.set_copy(b_ent, ChildOrder(u32::try_from(bi).unwrap_or(0)))?;
            world.relate::<ChildOf>(b_ent, root)?;

            match block {
                Block::Bar { name, backed, .. } => {
                    let wm = world.spawn()?;
                    world.set(wm, copy_words_in(name.as_slice(), alloc)?)?;
                    world.add_tag_typed::<SiteWordmark>(wm)?;
                    world.relate::<ChildOf>(wm, b_ent)?;

                    if !backed.is_empty() {
                        let bk = world.spawn()?;
                        world.set(bk, copy_words_in(backed.as_slice(), alloc)?)?;
                        world.add_tag_typed::<SiteBacked>(bk)?;
                        world.relate::<ChildOf>(bk, b_ent)?;
                    }
                }
                Block::Title { words } => {
                    world.set(b_ent, copy_words_in(words.as_slice(), alloc)?)?;
                    world.add_tag_typed::<SiteHeroTitle>(b_ent)?;
                }
                Block::Label { words } => {
                    world.set(b_ent, copy_words_in(words.as_slice(), alloc)?)?;
                }
                Block::Command { words, copy, button, .. } => {
                    world.set(b_ent, copy_words_in(words.as_slice(), alloc)?)?;
                    world.set(b_ent, SiteCopyText(OwnString::try_from_str_in(copy.as_str(), alloc)?))?;
                    if !first_cmd_tagged {
                        first_cmd_tagged = true;
                        world.add_tag_typed::<SiteInstallCmd>(b_ent)?;
                        let dom = extract_domain_from_copy(copy.as_str());
                        if !dom.is_empty() {
                            domain_str = Some(dom);
                        }
                        if !button.is_empty() {
                            world.set(copy_button, copy_words_in(button.as_slice(), alloc)?)?;
                            world.add_tag_typed::<SiteCopyLabel>(copy_button)?;
                        }
                    }
                }
                Block::List { items } | Block::Soon { items, .. } => {
                    world.add_tag(b_ent, ordered_tag)?;
                    let is_primary_list = matches!(block, Block::List { .. }) && !first_list_tagged;
                    if is_primary_list {
                        first_list_tagged = true;
                        world.add_tag_typed::<SiteFeatureList>(b_ent)?;
                    }
                    // Iterative walk over nested items in exact document order (push in reverse onto LIFO stack).
                    let mut stack: OwnVec<(usize, Entity, u32), A> = OwnVec::new_in(alloc);
                    let mut ri = items.len();
                    while ri > 0 {
                        ri -= 1;
                        if let Some(&idx) = items.get(ri) {
                            stack
                                .try_push((idx, b_ent, u32::try_from(ri).unwrap_or(0)))
                                .map_err(|r| SiteError::Alloc(r.why))?;
                        }
                    }
                    while let Some((item_idx, parent_ent, sibling_ord)) = stack.pop() {
                        let it_ent = world.spawn()?;
                        world.set_copy(it_ent, SiteNodeKind::ListItem(u32::try_from(item_idx).unwrap_or(0)))?;
                        world.set_copy(it_ent, ChildOrder(sibling_ord))?;
                        world.relate::<ChildOf>(it_ent, parent_ent)?;
                        if let Some(node) = self.items_arena.get(item_idx) {
                            world.set(it_ent, copy_words_in(node.words.as_slice(), alloc)?)?;
                            if is_primary_list && parent_ent == b_ent {
                                world.add_tag_typed::<SiteFeatureItem>(it_ent)?;
                            }
                            let mut ci = node.children.len();
                            while ci > 0 {
                                ci -= 1;
                                if let Some(&ch) = node.children.get(ci) {
                                    stack
                                        .try_push((ch, it_ent, u32::try_from(ci).unwrap_or(0)))
                                        .map_err(|r| SiteError::Alloc(r.why))?;
                                }
                            }
                        }
                    }
                }
                Block::Sign { closing, .. } => {
                    world.set(b_ent, copy_words_in(closing.as_slice(), alloc)?)?;
                }
                Block::Notes { .. } | Block::Footer { .. } => {}
            }
        }

        let dom_owned = OwnString::try_from_str_in(domain_str.unwrap_or(""), alloc)?;
        let _ = world.set_singleton(SiteMeta { domain: dom_owned })?;

        let mut link_entities = OwnVec::try_with_capacity_in(self.links.len(), alloc)?;
        for i in 0..self.links.len() {
            let e = world.spawn()?;
            world.set_copy(e, SiteNodeKind::Link(u32::try_from(i).unwrap_or(0)))?;
            world.relate::<ChildOf>(e, root)?;
            link_entities.try_push(e).map_err(|r| SiteError::Alloc(r.why))?;
        }

        let mut part_entities = OwnVec::new_in(alloc);
        for block in self.blocks.as_slice() {
            if let Block::Command { parts, .. } = block {
                for i in 0..parts.len() {
                    let e = world.spawn()?;
                    world.set_copy(e, SiteNodeKind::CommandPart(u32::try_from(i).unwrap_or(0)))?;
                    world.relate::<ChildOf>(e, root)?;
                    part_entities.try_push(e).map_err(|r| SiteError::Alloc(r.why))?;
                }
            }
        }

        let find_bar = world.spawn_named("BlazeSite::FindBar")?;
        world.set_copy(find_bar, SiteNodeKind::FindBar)?;
        world.relate::<ChildOf>(find_bar, root)?;

        Ok(SiteEcsHandles { root, copy_button, find_bar, link_entities, part_entities })
    }

    /// Parse `page.<hash>.json` iteratively using zero-AST `blaze_json::Reader`.
    pub fn parse_json(bytes: &[u8], alloc: A) -> Result<Self, SiteError> {
        let mut r = Reader::new_in(bytes, Limits::SHIPPED, alloc).map_err(|_| SiteError::JsonParse)?;
        let mut model = Self::empty_in(alloc);

        expect_event(&mut r, Event::StartMap)?;
        let mut key_buf = OwnString::new_in(alloc);
        while let Some(ev) = next_ev(&mut r)? {
            match ev {
                Event::EndMap => break,
                Event::Key(k) => {
                    decode_str_into(k, &mut key_buf)?;
                    match key_buf.as_str() {
                        "version" => {
                            let v = read_u32(&mut r)?;
                            if v != 1 {
                                return Err(SiteError::InvalidModel);
                            }
                        }
                        "table" => {
                            read_string_into(&mut r, &mut model.table)?;
                        }
                        "blocks" => {
                            parse_blocks(&mut r, &mut model.blocks, &mut model.items_arena, alloc)?;
                        }
                        "links" => {
                            parse_links(&mut r, &mut model.links, alloc)?;
                        }
                        "tips" => {
                            parse_tips(&mut r, &mut model.tips, alloc)?;
                        }
                        "scramble" => {
                            parse_scramble(&mut r, &mut model.scramble_note, &mut model.scramble_tip, alloc)?;
                        }
                        _ => skip_value(&mut r)?,
                    }
                }
                _ => return Err(SiteError::JsonParse),
            }
        }
        Ok(model)
    }
}

// =============================================================================
// Iterative `blaze_json::Reader` Helpers (Zero Recursion, Zero Stops!)
// =============================================================================

fn next_ev<'t, A: Allocator>(r: &mut Reader<'t, A>) -> Result<Option<Event<'t>>, SiteError> {
    r.next().map(|opt| opt.map(|i| i.event)).map_err(|_| SiteError::JsonParse)
}

fn expect_event<A: Allocator>(r: &mut Reader<'_, A>, expected: Event<'_>) -> Result<(), SiteError> {
    match next_ev(r)? {
        Some(ev) if ev == expected => Ok(()),
        _ => Err(SiteError::JsonParse),
    }
}

fn decode_str_into<A: Allocator>(s: Str<'_>, out: &mut OwnString<A>) -> Result<(), SiteError> {
    out.clear();
    if let Some(plain) = s.as_plain() {
        out.try_push_str(plain)?;
        return Ok(());
    }
    // `Str::decode_into` handles JSON escapes (surrogates, `\u`, etc.); reserve generous capacity first.
    out.try_reserve(s.raw().len())?;
    s.decode_into(out);
    Ok(())
}

fn read_string_into<A: Allocator>(r: &mut Reader<'_, A>, out: &mut OwnString<A>) -> Result<(), SiteError> {
    match next_ev(r)? {
        Some(Event::Text(s)) => decode_str_into(s, out),
        _ => Err(SiteError::JsonParse),
    }
}

fn read_f32<A: Allocator>(r: &mut Reader<'_, A>) -> Result<f32, SiteError> {
    match next_ev(r)? {
        Some(Event::Float(f)) => Ok(f64_to_f32(f)),
        Some(Event::Int(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(u64_to_f32(u))
            } else if let Some(i) = n.as_i64() {
                let neg = i < 0;
                let mag = i.unsigned_abs();
                let v = u64_to_f32(mag);
                Ok(if neg { -v } else { v })
            } else {
                Err(SiteError::JsonParse)
            }
        }
        _ => Err(SiteError::JsonParse),
    }
}

fn read_u32<A: Allocator>(r: &mut Reader<'_, A>) -> Result<u32, SiteError> {
    match next_ev(r)? {
        Some(Event::Int(n)) => n.as_u64().and_then(|u| u32::try_from(u).ok()).ok_or(SiteError::JsonParse),
        _ => Err(SiteError::JsonParse),
    }
}

fn read_bool<A: Allocator>(r: &mut Reader<'_, A>) -> Result<bool, SiteError> {
    match next_ev(r)? {
        Some(Event::Bool(b)) => Ok(b),
        _ => Err(SiteError::JsonParse),
    }
}

/// Iteratively skip any single JSON value (scalar, list, or map).
fn skip_value<A: Allocator>(r: &mut Reader<'_, A>) -> Result<(), SiteError> {
    let mut depth: usize = 0;
    loop {
        let Some(ev) = next_ev(r)? else {
            return Err(SiteError::JsonParse);
        };
        match ev {
            Event::StartList | Event::StartMap => depth += 1,
            Event::EndList | Event::EndMap => {
                if depth == 0 {
                    return Err(SiteError::JsonParse);
                }
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Event::Key(_) => {}
            Event::Null | Event::Bool(_) | Event::Int(_) | Event::Float(_) | Event::Text(_) => {
                if depth == 0 {
                    return Ok(());
                }
            }
        }
    }
}

fn parse_word_list<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    out: &mut OwnVec<Word<A>, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartList)?;
    let mut key = OwnString::new_in(alloc);
    let mut val_s = OwnString::new_in(alloc);
    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndList => break,
            Event::StartMap => {
                let mut w = Word::empty_in(RoleName::Sub, alloc);
                while let Some(fev) = next_ev(r)? {
                    match fev {
                        Event::EndMap => break,
                        Event::Key(k) => {
                            decode_str_into(k, &mut key)?;
                            match key.as_str() {
                                "text" => read_string_into(r, &mut w.text)?,
                                "role" => {
                                    read_string_into(r, &mut val_s)?;
                                    w.role = RoleName::from_str(val_s.as_str()).unwrap_or(RoleName::Sub);
                                }
                                "glyphs" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(gev) = next_ev(r)? {
                                        match gev {
                                            Event::EndList => break,
                                            Event::Int(n) => {
                                                let gi = n.as_i64().and_then(|i| i32::try_from(i).ok()).unwrap_or(-1);
                                                w.glyphs.try_push(gi).map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                "xs" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(xev) = next_ev(r)? {
                                        match xev {
                                            Event::EndList => break,
                                            Event::Float(f) => {
                                                w.xs.try_push(f64_to_f32(f)).map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            Event::Int(n) => {
                                                let v = n.as_u64().map(u64_to_f32).unwrap_or(0.0);
                                                w.xs.try_push(v).map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                "adv" => w.adv = read_f32(r)?,
                                "space" => w.space = read_f32(r)?,
                                "link" => w.link = Some(read_u32(r)?),
                                "chip" => w.chip = Some(read_u32(r)?),
                                "chipKind" => {
                                    read_string_into(r, &mut val_s)?;
                                    w.chip_kind = Some(ChipKind::from_str(val_s.as_str()));
                                }
                                "deco" => w.deco = read_bool(r)?,
                                "image" => {
                                    read_string_into(r, &mut val_s)?;
                                    w.image = ImageName::from_str(val_s.as_str());
                                }
                                "rise" => w.rise = read_f32(r)?,
                                "drop" => w.drop = read_f32(r)?,
                                "raise" => w.raise = read_f32(r)?,
                                "part" => w.part = Some(read_u32(r)?),
                                "scramble" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(sev) = next_ev(r)? {
                                        match sev {
                                            Event::EndList => break,
                                            Event::Int(n) => {
                                                let si = n.as_u64().and_then(|u| u32::try_from(u).ok()).unwrap_or(0);
                                                w.scramble.try_push(si).map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                _ => skip_value(r)?,
                            }
                        }
                        _ => return Err(SiteError::JsonParse),
                    }
                }
                out.try_push(w).map_err(|e| SiteError::Alloc(e.why))?;
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

/// Parse nested `items: Item[]` iteratively (explicit parent stack, zero recursion!).
fn parse_item_tree<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    arena: &mut OwnVec<ItemNode<A>, A>,
    root_out: &mut OwnVec<usize, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartList)?;
    // Stack of `Option<usize>` parent indices (`None` = top-level `root_out`).
    let mut parent_stack: OwnVec<Option<usize>, A> = OwnVec::new_in(alloc);
    parent_stack.try_push(None).map_err(|e| SiteError::Alloc(e.why))?;
    let mut key = OwnString::new_in(alloc);

    while let Some(parent_opt) = parent_stack.last().copied() {
        let Some(ev) = next_ev(r)? else {
            return Err(SiteError::JsonParse);
        };
        match ev {
            Event::EndList => {
                let _ = parent_stack.pop();
                // If we just finished a `children: [...]` list inside an item map, resume reading
                // remaining fields of that parent item map!
                if let Some(p_idx) = parent_opt {
                    let mut in_parent = Some(p_idx);
                    while let Some(cur_idx) = in_parent {
                        match next_ev(r)? {
                            Some(Event::EndMap) => {
                                in_parent = None;
                            }
                            Some(Event::Key(k)) => {
                                decode_str_into(k, &mut key)?;
                                match key.as_str() {
                                    "words" => {
                                        if let Some(node) = arena.get_mut(cur_idx) {
                                            parse_word_list(r, &mut node.words, alloc)?;
                                        }
                                    }
                                    "race" => {
                                        let race = parse_race(r, alloc)?;
                                        if let Some(node) = arena.get_mut(cur_idx) {
                                            node.race = Some(race);
                                        }
                                    }
                                    "children" => {
                                        expect_event(r, Event::StartList)?;
                                        parent_stack.try_push(Some(cur_idx)).map_err(|e| SiteError::Alloc(e.why))?;
                                        in_parent = None;
                                    }
                                    _ => skip_value(r)?,
                                }
                            }
                            _ => return Err(SiteError::JsonParse),
                        }
                    }
                }
            }
            Event::StartMap => {
                let my_idx = arena.len();
                arena
                    .try_push(ItemNode {
                        words: OwnVec::new_in(alloc),
                        children: OwnVec::new_in(alloc),
                        race: None,
                    })
                    .map_err(|e| SiteError::Alloc(e.why))?;
                match parent_opt {
                    None => root_out.try_push(my_idx).map_err(|e| SiteError::Alloc(e.why))?,
                    Some(p_idx) => {
                        if let Some(p) = arena.get_mut(p_idx) {
                            p.children.try_push(my_idx).map_err(|e| SiteError::Alloc(e.why))?;
                        }
                    }
                }
                // Read item map keys until EndMap or descending into `children`.
                loop {
                    match next_ev(r)? {
                        Some(Event::EndMap) => break,
                        Some(Event::Key(k)) => {
                            decode_str_into(k, &mut key)?;
                            match key.as_str() {
                                "words" => {
                                    if let Some(node) = arena.get_mut(my_idx) {
                                        parse_word_list(r, &mut node.words, alloc)?;
                                    }
                                }
                                "race" => {
                                    let race = parse_race(r, alloc)?;
                                    if let Some(node) = arena.get_mut(my_idx) {
                                        node.race = Some(race);
                                    }
                                }
                                "children" => {
                                    expect_event(r, Event::StartList)?;
                                    parent_stack.try_push(Some(my_idx)).map_err(|e| SiteError::Alloc(e.why))?;
                                    break;
                                }
                                _ => skip_value(r)?,
                            }
                        }
                        _ => return Err(SiteError::JsonParse),
                    }
                }
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

fn parse_race<A: Allocator + Copy>(r: &mut Reader<'_, A>, alloc: A) -> Result<Race<A>, SiteError> {
    expect_event(r, Event::StartMap)?;
    let mut rows = OwnVec::new_in(alloc);
    let mut title = OwnString::new_in(alloc);
    let mut key = OwnString::new_in(alloc);
    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndMap => break,
            Event::Key(k) => {
                decode_str_into(k, &mut key)?;
                match key.as_str() {
                    "title" => read_string_into(r, &mut title)?,
                    "rows" => {
                        expect_event(r, Event::StartList)?;
                        while let Some(rev) = next_ev(r)? {
                            match rev {
                                Event::EndList => break,
                                Event::StartMap => {
                                    let mut row = RaceRow {
                                        who: OwnVec::new_in(alloc),
                                        took: OwnVec::new_in(alloc),
                                        share: 0.0,
                                        lead: false,
                                    };
                                    while let Some(fev) = next_ev(r)? {
                                        match fev {
                                            Event::EndMap => break,
                                            Event::Key(rk) => {
                                                decode_str_into(rk, &mut key)?;
                                                match key.as_str() {
                                                    "who" => parse_word_list(r, &mut row.who, alloc)?,
                                                    "took" => parse_word_list(r, &mut row.took, alloc)?,
                                                    "share" => row.share = read_f32(r)?,
                                                    "lead" => row.lead = read_bool(r)?,
                                                    _ => skip_value(r)?,
                                                }
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                    rows.try_push(row).map_err(|e| SiteError::Alloc(e.why))?;
                                }
                                _ => return Err(SiteError::JsonParse),
                            }
                        }
                    }
                    _ => skip_value(r)?,
                }
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(Race { rows, title })
}

fn parse_blocks<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    blocks: &mut OwnVec<Block<A>, A>,
    arena: &mut OwnVec<ItemNode<A>, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartList)?;
    let mut key = OwnString::new_in(alloc);
    let mut kind_s = OwnString::new_in(alloc);
    let mut img_s = OwnString::new_in(alloc);

    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndList => break,
            Event::StartMap => {
                kind_s.clear();
                let mut name = OwnVec::new_in(alloc);
                let mut links = OwnVec::new_in(alloc);
                let mut backed = OwnVec::new_in(alloc);
                let mut words = OwnVec::new_in(alloc);
                let mut label = OwnVec::new_in(alloc);
                let mut parts: OwnVec<OwnString<A>, A> = OwnVec::new_in(alloc);
                let mut copy = OwnString::new_in(alloc);
                let mut button = OwnVec::new_in(alloc);
                let mut copied = OwnVec::new_in(alloc);
                let mut items = OwnVec::new_in(alloc);
                let mut closing = OwnVec::new_in(alloc);
                let mut contact = OwnVec::new_in(alloc);
                let mut notes = OwnVec::new_in(alloc);
                let mut image = ImageName::Signature;

                while let Some(fev) = next_ev(r)? {
                    match fev {
                        Event::EndMap => break,
                        Event::Key(k) => {
                            decode_str_into(k, &mut key)?;
                            match key.as_str() {
                                "kind" => read_string_into(r, &mut kind_s)?,
                                "name" => parse_word_list(r, &mut name, alloc)?,
                                "links" => parse_word_list(r, &mut links, alloc)?,
                                "backed" => parse_word_list(r, &mut backed, alloc)?,
                                "words" => parse_word_list(r, &mut words, alloc)?,
                                "label" => parse_word_list(r, &mut label, alloc)?,
                                "parts" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(pev) = next_ev(r)? {
                                        match pev {
                                            Event::EndList => break,
                                            Event::Text(ps) => {
                                                let mut s = OwnString::new_in(alloc);
                                                decode_str_into(ps, &mut s)?;
                                                parts.try_push(s).map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                "copy" => read_string_into(r, &mut copy)?,
                                "button" => parse_word_list(r, &mut button, alloc)?,
                                "copied" => parse_word_list(r, &mut copied, alloc)?,
                                "items" => parse_item_tree(r, arena, &mut items, alloc)?,
                                "image" => {
                                    read_string_into(r, &mut img_s)?;
                                    image = ImageName::from_str(img_s.as_str()).unwrap_or(ImageName::Signature);
                                }
                                "closing" => parse_word_list(r, &mut closing, alloc)?,
                                "contact" => parse_word_list(r, &mut contact, alloc)?,
                                "notes" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(nev) = next_ev(r)? {
                                        match nev {
                                            Event::EndList => break,
                                            Event::StartMap => {
                                                let mut nid = OwnString::new_in(alloc);
                                                let mut nwords = OwnVec::new_in(alloc);
                                                while let Some(nfev) = next_ev(r)? {
                                                    match nfev {
                                                        Event::EndMap => break,
                                                        Event::Key(nk) => {
                                                            decode_str_into(nk, &mut key)?;
                                                            match key.as_str() {
                                                                "id" => read_string_into(r, &mut nid)?,
                                                                "words" => parse_word_list(r, &mut nwords, alloc)?,
                                                                _ => skip_value(r)?,
                                                            }
                                                        }
                                                        _ => return Err(SiteError::JsonParse),
                                                    }
                                                }
                                                notes
                                                    .try_push(NoteEntry { id: nid, words: nwords })
                                                    .map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                _ => skip_value(r)?,
                            }
                        }
                        _ => return Err(SiteError::JsonParse),
                    }
                }

                let blk = match kind_s.as_str() {
                    "bar" => Block::Bar { name, links, backed },
                    "title" => Block::Title { words },
                    "label" => Block::Label { words },
                    "command" => Block::Command { words, parts, copy, button, copied },
                    "list" => Block::List { items },
                    "soon" => Block::Soon { label, items },
                    "sign" => Block::Sign { image, closing, words, contact },
                    "notes" => Block::Notes { notes },
                    "footer" => Block::Footer { links },
                    _ => return Err(SiteError::InvalidModel),
                };
                blocks.try_push(blk).map_err(|e| SiteError::Alloc(e.why))?;
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

fn parse_links<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    out: &mut OwnVec<Link<A>, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartList)?;
    let mut key = OwnString::new_in(alloc);
    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndList => break,
            Event::StartMap => {
                let mut href = OwnString::new_in(alloc);
                let mut tip: Option<OwnString<A>> = None;
                let mut note: Option<OwnString<A>> = None;
                while let Some(fev) = next_ev(r)? {
                    match fev {
                        Event::EndMap => break,
                        Event::Key(k) => {
                            decode_str_into(k, &mut key)?;
                            match key.as_str() {
                                "href" => read_string_into(r, &mut href)?,
                                "tip" => {
                                    let mut s = OwnString::new_in(alloc);
                                    read_string_into(r, &mut s)?;
                                    tip = Some(s);
                                }
                                "note" => {
                                    let mut s = OwnString::new_in(alloc);
                                    read_string_into(r, &mut s)?;
                                    note = Some(s);
                                }
                                _ => skip_value(r)?,
                            }
                        }
                        _ => return Err(SiteError::JsonParse),
                    }
                }
                out.try_push(Link { href, tip, note }).map_err(|e| SiteError::Alloc(e.why))?;
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

fn parse_tips<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    out: &mut OwnVec<TipEntry<A>, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartMap)?;
    let mut fkey = OwnString::new_in(alloc);
    let mut kind_s = OwnString::new_in(alloc);
    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndMap => break,
            Event::Key(tkey) => {
                let mut entry_key = OwnString::new_in(alloc);
                decode_str_into(tkey, &mut entry_key)?;
                expect_event(r, Event::StartMap)?;
                kind_s.clear();
                let mut title: Option<OwnVec<Word<A>, A>> = None;
                let mut body = OwnVec::new_in(alloc);
                let mut rows = OwnVec::new_in(alloc);
                while let Some(fev) = next_ev(r)? {
                    match fev {
                        Event::EndMap => break,
                        Event::Key(k) => {
                            decode_str_into(k, &mut fkey)?;
                            match fkey.as_str() {
                                "kind" => read_string_into(r, &mut kind_s)?,
                                "title" => {
                                    let mut t = OwnVec::new_in(alloc);
                                    parse_word_list(r, &mut t, alloc)?;
                                    title = Some(t);
                                }
                                "body" => parse_word_list(r, &mut body, alloc)?,
                                "rows" => {
                                    expect_event(r, Event::StartList)?;
                                    while let Some(rev) = next_ev(r)? {
                                        match rev {
                                            Event::EndList => break,
                                            Event::StartMap => {
                                                let mut says = OwnVec::new_in(alloc);
                                                let mut does = OwnVec::new_in(alloc);
                                                while let Some(rfev) = next_ev(r)? {
                                                    match rfev {
                                                        Event::EndMap => break,
                                                        Event::Key(rk) => {
                                                            decode_str_into(rk, &mut fkey)?;
                                                            match fkey.as_str() {
                                                                "says" => parse_word_list(r, &mut says, alloc)?,
                                                                "does" => parse_word_list(r, &mut does, alloc)?,
                                                                _ => skip_value(r)?,
                                                            }
                                                        }
                                                        _ => return Err(SiteError::JsonParse),
                                                    }
                                                }
                                                rows.try_push(TipRow { says, does })
                                                    .map_err(|e| SiteError::Alloc(e.why))?;
                                            }
                                            _ => return Err(SiteError::JsonParse),
                                        }
                                    }
                                }
                                _ => skip_value(r)?,
                            }
                        }
                        _ => return Err(SiteError::JsonParse),
                    }
                }
                let kind = if kind_s.as_str() == "rows" {
                    TipKind::Rows { rows }
                } else {
                    TipKind::Text { title, body }
                };
                out.try_push(TipEntry { key: entry_key, kind }).map_err(|e| SiteError::Alloc(e.why))?;
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

fn parse_scramble<A: Allocator + Copy>(
    r: &mut Reader<'_, A>,
    note_out: &mut OwnVec<u32, A>,
    tip_out: &mut OwnVec<u32, A>,
    alloc: A,
) -> Result<(), SiteError> {
    expect_event(r, Event::StartMap)?;
    let mut key = OwnString::new_in(alloc);
    while let Some(ev) = next_ev(r)? {
        match ev {
            Event::EndMap => break,
            Event::Key(k) => {
                decode_str_into(k, &mut key)?;
                let target = match key.as_str() {
                    "note" => Some(&mut *note_out),
                    "tip" => Some(&mut *tip_out),
                    _ => None,
                };
                if let Some(dst) = target {
                    expect_event(r, Event::StartList)?;
                    while let Some(gev) = next_ev(r)? {
                        match gev {
                            Event::EndList => break,
                            Event::Int(n) => {
                                if let Some(u) = n.as_u64().and_then(|u| u32::try_from(u).ok()) {
                                    dst.try_push(u).map_err(|e| SiteError::Alloc(e.why))?;
                                }
                            }
                            _ => return Err(SiteError::JsonParse),
                        }
                    }
                } else {
                    skip_value(r)?;
                }
            }
            _ => return Err(SiteError::JsonParse),
        }
    }
    Ok(())
}

fn u64_to_f32(u: u64) -> f32 {
    let hi = u32::try_from(u >> 32).unwrap_or(0);
    let lo = u32::try_from(u & 0xffff_ffff).unwrap_or(0);
    gpu::math::to_f32(hi) * 4_294_967_296.0 + gpu::math::to_f32(lo)
}

fn f64_to_f32(v: f64) -> f32 {
    let bits = v.to_bits();
    let sign = u32::try_from((bits >> 63) & 1).unwrap_or(0) << 31;
    let exp = u32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if exp == 0x7ff {
        let nan_bit = if frac != 0 { 0x0040_0000 } else { 0 };
        return f32::from_bits(sign | 0x7f80_0000 | nan_bit);
    }
    if exp == 0 && frac == 0 {
        return f32::from_bits(sign);
    }
    let unbiased = i32::try_from(exp).unwrap_or(0) - 1023;
    let exp32 = unbiased + 127;
    if exp32 >= 255 {
        return f32::from_bits(sign | 0x7f80_0000);
    }
    if exp32 <= 0 {
        return f32::from_bits(sign);
    }
    let mant = u32::try_from(frac >> 29).unwrap_or(0);
    let round_bit = u32::try_from((frac >> 28) & 1).unwrap_or(0);
    let exp_bits = u32::try_from(exp32).unwrap_or(0) << 23;
    f32::from_bits(sign | (exp_bits + mant + round_bit))
}
