//! Semantic, accessible HTML projection of the live `blaze_ecs::World` (`ChildOf` +
//! `SiteNodeKind` + `DontFragment` tags) for browsers without WebGPU (or `?renderer=html`).
//!
//! Pure `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`, zero recursion, zero stops.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_ecs::{Entity, World};
use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::error::SiteError;
use crate::model::{
    Block, ChipKind, PageModel, SiteEcsHandles, SiteFocused, SiteHovered, SiteNodeKind, TipKind,
    Word,
};
use crate::palette::{ImageName, RoleName};

/// Project the mounted `blaze_ecs::World` scene graph directly into semantic HTML.
///
/// Walks `world.children_slice(handles.root)` (`ChildOf` relations) and iteratively traverses
/// nested `SiteNodeKind::ListItem` entities, annotating each DOM node with its live ECS entity
/// slot (`data-ecs-entity`) and `DontFragment` tag attributes, plus `<template data-tip-key>`
/// hover-card definitions for every tip in `model.tips`.
///
/// Returns the number of ECS entities projected into the HTML tree.
pub fn project_ecs_html<A: Allocator + Copy + 'static>(
    world: &World<A>,
    handles: &SiteEcsHandles<A>,
    model: &PageModel<A>,
    dark: bool,
    out: &mut OwnString<A>,
    alloc: A,
) -> Result<u32, SiteError> {
    out.clear();
    let theme_name = if dark { "dark" } else { "light" };
    let mut projected: u32 = 1; // PageRoot

    let _ = write!(
        out,
        "<div class=\"page\" data-theme=\"{}\" data-ecs-entity=\"{}\" data-ecs-kind=\"page-root\">",
        theme_name,
        handles.root.slot(),
    );

    let root_children = world.children_slice(handles.root);
    let mut prev_was_label = false;
    let mut prev_was_command = false;

    for &ent in root_children {
        let Some(&kind) = world.get::<SiteNodeKind>(ent)? else { continue };
        let SiteNodeKind::Block(bi) = kind else { continue };
        let Some(block) = model.blocks.get(usize::try_from(bi).unwrap_or(usize::MAX)) else {
            continue;
        };
        projected = projected.saturating_add(1);

        let cur_is_label = matches!(block, Block::Label { .. });
        let cur_is_command = matches!(block, Block::Command { .. });

        match block {
            Block::Bar { name, links, backed } => {
                let _ = write!(
                    out,
                    "<header class=\"bar\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:bar\">",
                    ent.slot()
                );
                out.try_push_str("<div class=\"bar-left\"><span class=\"wordmark\">")?;
                emit_words_html(world, handles, model, name.as_slice(), out)?;
                out.try_push_str("</span><span class=\"bar-div\" aria-hidden=\"true\"></span><span class=\"backed\">")?;
                emit_words_html(world, handles, model, backed.as_slice(), out)?;
                out.try_push_str("</span></div><nav class=\"bar-links\" aria-label=\"Primary\">")?;
                emit_words_html(world, handles, model, links.as_slice(), out)?;
                out.try_push_str("</nav></header>")?;
            }
            Block::Title { words } => {
                let _ = write!(
                    out,
                    "<h1 class=\"title\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:title\">",
                    ent.slot()
                );
                emit_words_html(world, handles, model, words.as_slice(), out)?;
                out.try_push_str("</h1>")?;
            }
            Block::Label { words } => {
                let _ = write!(
                    out,
                    "<p class=\"label\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:label\">",
                    ent.slot()
                );
                emit_words_html(world, handles, model, words.as_slice(), out)?;
                out.try_push_str("</p>")?;
            }
            Block::Command { words, copy, button, .. } => {
                let cls = if prev_was_label { "cmd-box after-label" } else { "cmd-box" };
                let _ = write!(
                    out,
                    "<div class=\"{}\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:command\">",
                    cls,
                    ent.slot()
                );
                out.try_push_str("<code class=\"cmd-line\">")?;
                emit_words_html(world, handles, model, words.as_slice(), out)?;
                out.try_push_str("</code>")?;

                let btn_ent = handles.copy_button;
                let btn_hov = world.has_tag_typed::<SiteHovered>(btn_ent).unwrap_or(false);
                let _ = write!(
                    out,
                    "<button type=\"button\" class=\"copy-btn\" data-ecs-entity=\"{}\" data-ecs-kind=\"copy-button\"{} data-copy=\"",
                    btn_ent.slot(),
                    if btn_hov { " data-ecs-hovered=\"1\"" } else { "" }
                );
                push_escaped_attr(copy.as_str(), out)?;
                out.try_push_str("\">")?;
                emit_words_html(world, handles, model, button.as_slice(), out)?;
                out.try_push_str("</button></div>")?;
                projected = projected.saturating_add(1);
            }
            Block::List { .. } => {
                let cls = if prev_was_command { "list after-cmd" } else { "list" };
                let _ = write!(
                    out,
                    "<ul class=\"{}\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:list\">",
                    cls,
                    ent.slot()
                );
                let n = emit_ecs_item_tree(world, handles, model, ent, false, out, alloc)?;
                projected = projected.saturating_add(n);
                out.try_push_str("</ul>")?;
            }
            Block::Soon { label, .. } => {
                let _ = write!(
                    out,
                    "<section class=\"soon\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:soon\">",
                    ent.slot()
                );
                out.try_push_str("<p class=\"label\">")?;
                emit_words_html(world, handles, model, label.as_slice(), out)?;
                out.try_push_str("</p><ul class=\"list list-soon\">")?;
                let n = emit_ecs_item_tree(world, handles, model, ent, true, out, alloc)?;
                projected = projected.saturating_add(n);
                out.try_push_str("</ul></section>")?;
            }
            Block::Sign { image, closing, words, contact } => {
                let _ = write!(
                    out,
                    "<section class=\"sign\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:sign\">",
                    ent.slot()
                );
                if !closing.is_empty() {
                    out.try_push_str("<p class=\"closing\">")?;
                    emit_words_html(world, handles, model, closing.as_slice(), out)?;
                    out.try_push_str("</p>")?;
                }
                out.try_push_str("<div class=\"sig-svg\" aria-hidden=\"true\">")?;
                out.try_push_str(image.svg_source())?;
                out.try_push_str("</div><p class=\"sign-line\">")?;
                emit_words_html(world, handles, model, words.as_slice(), out)?;
                out.try_push_str("</p>")?;
                if !contact.is_empty() {
                    out.try_push_str("<p class=\"sign-contact\">")?;
                    emit_words_html(world, handles, model, contact.as_slice(), out)?;
                    out.try_push_str("</p>")?;
                }
                out.try_push_str("</section>")?;
            }
            Block::Notes { notes } => {
                let _ = write!(
                    out,
                    "<section class=\"notes\" id=\"notes\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:notes\">",
                    ent.slot()
                );
                out.try_push_str("<hr class=\"note-rule\" />")?;
                for n in notes.as_slice() {
                    out.try_push_str("<p class=\"note\" id=\"note-")?;
                    push_escaped_attr(n.id.as_str(), out)?;
                    out.try_push_str("\" data-note-id=\"")?;
                    push_escaped_attr(n.id.as_str(), out)?;
                    out.try_push_str("\">")?;
                    emit_words_html(world, handles, model, n.words.as_slice(), out)?;
                    out.try_push_str("</p>")?;
                }
                out.try_push_str("</section>")?;
            }
            Block::Footer { links } => {
                let _ = write!(
                    out,
                    "<footer class=\"footer\" data-ecs-entity=\"{}\" data-ecs-kind=\"block:footer\">",
                    ent.slot()
                );
                emit_words_html(world, handles, model, links.as_slice(), out)?;
                out.try_push_str("</footer>")?;
            }
        }

        prev_was_label = cur_is_label;
        prev_was_command = cur_is_command;
    }

    // Also annotate the ECS FindBar entity & projection status badge
    let find_ent = handles.find_bar;
    let find_foc = world.has_tag_typed::<SiteFocused>(find_ent).unwrap_or(false);
    let _ = write!(
        out,
        "<div class=\"ecs-badge\" data-ecs-entity=\"{}\" data-ecs-kind=\"find-bar\"{}>HTML ECS projection &middot; {} entities</div>",
        find_ent.slot(),
        if find_foc { " data-ecs-focused=\"1\"" } else { "" },
        projected.saturating_add(1),
    );
    projected = projected.saturating_add(1);

    // Emit hover-card `<template data-tip-key="...">` bank so all tips work identically without WebGPU
    out.try_push_str("<div class=\"tips-bank\" hidden>")?;
    for tip in model.tips.as_slice() {
        out.try_push_str("<template data-tip-key=\"")?;
        push_escaped_attr(tip.key.as_str(), out)?;
        out.try_push_str("\">")?;
        match &tip.kind {
            TipKind::Text { title, body } => {
                if let Some(tw) = title {
                    out.try_push_str("<div class=\"tip-title\">")?;
                    emit_words_html(world, handles, model, tw.as_slice(), out)?;
                    out.try_push_str("</div>")?;
                }
                out.try_push_str("<div class=\"tip-body\">")?;
                emit_words_html(world, handles, model, body.as_slice(), out)?;
                out.try_push_str("</div>")?;
            }
            TipKind::Rows { rows } => {
                out.try_push_str("<div class=\"tip-rows\">")?;
                for r in rows.as_slice() {
                    out.try_push_str("<div class=\"tip-row\"><span class=\"tip-says\">")?;
                    emit_words_html(world, handles, model, r.says.as_slice(), out)?;
                    out.try_push_str("</span><span class=\"tip-does\">")?;
                    emit_words_html(world, handles, model, r.does.as_slice(), out)?;
                    out.try_push_str("</span></div>")?;
                }
                out.try_push_str("</div>")?;
            }
        }
        out.try_push_str("</template>")?;
    }
    out.try_push_str("</div></div>")?;

    Ok(projected)
}

/// Iteratively walk ECS `ChildOf` `SiteNodeKind::ListItem` children into nested `<li>` / `<ul>` HTML.
fn emit_ecs_item_tree<A: Allocator + Copy + 'static>(
    world: &World<A>,
    handles: &SiteEcsHandles<A>,
    model: &PageModel<A>,
    parent_block: Entity,
    ring_top: bool,
    out: &mut OwnString<A>,
    alloc: A,
) -> Result<u32, SiteError> {
    enum Step {
        Enter { ent: Entity, level: u8 },
        CloseSubUl,
        CloseLi,
    }
    let mut stack: OwnVec<Step, A> = OwnVec::new_in(alloc);
    let top_children = world.children_slice(parent_block);
    let mut ri = top_children.len();
    while ri > 0 {
        ri -= 1;
        if let Some(&ch) = top_children.get(ri) {
            stack.try_push(Step::Enter { ent: ch, level: 0 }).map_err(|r| SiteError::Alloc(r.why))?;
        }
    }

    let mut count: u32 = 0;
    while let Some(step) = stack.pop() {
        match step {
            Step::CloseLi => out.try_push_str("</li>")?,
            Step::CloseSubUl => out.try_push_str("</ul>")?,
            Step::Enter { ent, level } => {
                let Some(&SiteNodeKind::ListItem(item_idx)) = world.get::<SiteNodeKind>(ent)? else {
                    continue;
                };
                let Some(node) = model.items_arena.get(usize::try_from(item_idx).unwrap_or(usize::MAX)) else {
                    continue;
                };
                count = count.saturating_add(1);
                let ecs_children = world.children_slice(ent);
                let has_children = !ecs_children.is_empty();
                let cls = if level == 0 {
                    if ring_top { "item item-ring" } else { "item" }
                } else if has_children {
                    "sub sub-head"
                } else if level == 1 {
                    "sub"
                } else {
                    "sub sub-deep"
                };
                let _ = write!(
                    out,
                    "<li class=\"{}\" data-ecs-entity=\"{}\" data-ecs-kind=\"list-item\"><div class=\"item-line\">",
                    cls,
                    ent.slot()
                );
                emit_words_html(world, handles, model, node.words.as_slice(), out)?;
                out.try_push_str("</div>")?;

                stack.try_push(Step::CloseLi).map_err(|r| SiteError::Alloc(r.why))?;
                if has_children {
                    out.try_push_str("<ul class=\"sub-list\">")?;
                    stack.try_push(Step::CloseSubUl).map_err(|r| SiteError::Alloc(r.why))?;
                    let mut ci = ecs_children.len();
                    while ci > 0 {
                        ci -= 1;
                        if let Some(&ch) = ecs_children.get(ci) {
                            stack
                                .try_push(Step::Enter { ent: ch, level: level.saturating_add(1) })
                                .map_err(|r| SiteError::Alloc(r.why))?;
                        }
                    }
                }
            }
        }
    }
    Ok(count)
}

fn emit_words_html<A: Allocator + Copy + 'static>(
    world: &World<A>,
    handles: &SiteEcsHandles<A>,
    model: &PageModel<A>,
    words: &[Word<A>],
    out: &mut OwnString<A>,
) -> Result<(), SiteError> {
    let mut i = 0usize;
    while i < words.len() {
        let Some(w) = words.get(i) else { break };

        // 1. Group consecutive words belonging to the same chip / badge
        if let Some(cid) = w.chip {
            let mut j = i;
            while j + 1 < words.len() && words.get(j + 1).and_then(|x| x.chip) == Some(cid) {
                j += 1;
            }
            let is_badge = (i..=j).any(|k| words.get(k).and_then(|x| x.chip_kind) == Some(ChipKind::Badge));
            let cls = if is_badge { "chip chip-badge" } else { "chip" };
            out.try_push_str("<code class=\"")?;
            out.try_push_str(cls)?;
            out.try_push_str("\">")?;
            let mut k = i;
            while k <= j {
                if let Some(cw) = words.get(k) {
                    if let Some(lid) = cw.link {
                        let mut m = k;
                        while m + 1 <= j && words.get(m + 1).and_then(|x| x.link) == Some(lid) {
                            m += 1;
                        }
                        emit_link_open(world, handles, model, lid, cw.role, out)?;
                        for t in k..=m {
                            if let Some(tw) = words.get(t) {
                                emit_single_word(tw, out)?;
                                if t < m && tw.space > 0.0 {
                                    out.try_push(' ')?;
                                }
                            }
                        }
                        out.try_push_str("</a>")?;
                        if m < j && words.get(m).is_some_and(|x| x.space > 0.0) {
                            out.try_push(' ')?;
                        }
                        k = m + 1;
                        continue;
                    }
                    emit_single_word(cw, out)?;
                    if k < j && cw.space > 0.0 {
                        out.try_push(' ')?;
                    }
                }
                k += 1;
            }
            out.try_push_str("</code>")?;
            if j + 1 < words.len() && words.get(j).is_some_and(|x| x.space > 0.0) {
                out.try_push(' ')?;
            }
            i = j + 1;
            continue;
        }

        // 2. Group consecutive words belonging to the same command part (`curl`, `-fsSL`, ...)
        if let Some(pid) = w.part {
            let mut j = i;
            while j + 1 < words.len() && words.get(j + 1).and_then(|x| x.part) == Some(pid) {
                j += 1;
            }
            let part_ent = handles.part_entities.get(usize::try_from(pid).unwrap_or(usize::MAX)).copied();
            out.try_push_str("<span class=\"cmd-part\"")?;
            if let Some(pe) = part_ent {
                let _ = write!(out, " data-ecs-entity=\"{}\"", pe.slot());
                if world.has_tag_typed::<SiteHovered>(pe).unwrap_or(false) {
                    out.try_push_str(" data-ecs-hovered=\"1\"")?;
                }
            }
            let _ = write!(out, " data-tip=\"part:{pid}\" tabindex=\"0\">");
            for k in i..=j {
                if let Some(pw) = words.get(k) {
                    emit_single_word(pw, out)?;
                    if k < j && pw.space > 0.0 {
                        out.try_push(' ')?;
                    }
                }
            }
            out.try_push_str("</span>")?;
            if j + 1 < words.len() && words.get(j).is_some_and(|x| x.space > 0.0) {
                out.try_push(' ')?;
            }
            i = j + 1;
            continue;
        }

        // 3. Group consecutive words belonging to the same link
        if let Some(lid) = w.link {
            let mut j = i;
            while j + 1 < words.len()
                && words.get(j + 1).and_then(|x| x.link) == Some(lid)
                && words.get(j + 1).and_then(|x| x.chip).is_none()
            {
                j += 1;
            }
            emit_link_open(world, handles, model, lid, w.role, out)?;
            for k in i..=j {
                if let Some(lw) = words.get(k) {
                    emit_single_word(lw, out)?;
                    if k < j && lw.space > 0.0 {
                        out.try_push(' ')?;
                    }
                }
            }
            out.try_push_str("</a>")?;
            if j + 1 < words.len() && words.get(j).is_some_and(|x| x.space > 0.0) {
                out.try_push(' ')?;
            }
            i = j + 1;
            continue;
        }

        // 4. Plain word or inline picture
        emit_single_word(w, out)?;
        if i + 1 < words.len() && w.space > 0.0 {
            out.try_push(' ')?;
        }
        i += 1;
    }
    Ok(())
}

fn emit_link_open<A: Allocator + Copy + 'static>(
    world: &World<A>,
    handles: &SiteEcsHandles<A>,
    model: &PageModel<A>,
    lid: u32,
    role: RoleName,
    out: &mut OwnString<A>,
) -> Result<(), SiteError> {
    let idx = usize::try_from(lid).unwrap_or(usize::MAX);
    let link = model.links.get(idx);
    let ent = handles.link_entities.get(idx).copied();
    let underline = role.spec().link.is_some_and(|l| l.underline);

    out.try_push_str("<a class=\"")?;
    out.try_push_str(if underline { "lnk lnk-u" } else { "lnk" })?;
    out.try_push_str("\" href=\"")?;
    if let Some(l) = link {
        if let Some(note) = &l.note {
            out.try_push_str("#note-")?;
            push_escaped_attr(note.as_str(), out)?;
        } else if !l.href.is_empty() {
            push_escaped_attr(l.href.as_str(), out)?;
        } else {
            out.try_push('#')?;
        }
    } else {
        out.try_push('#')?;
    }
    out.try_push('"')?;

    if let Some(l) = link {
        if l.note.is_none() && (l.href.as_str().starts_with("http://") || l.href.as_str().starts_with("https://")) {
            out.try_push_str(" target=\"_blank\" rel=\"noopener noreferrer\"")?;
        }
        if let Some(tip) = &l.tip {
            out.try_push_str(" data-tip=\"")?;
            push_escaped_attr(tip.as_str(), out)?;
            out.try_push('"')?;
        }
        if let Some(note) = &l.note {
            out.try_push_str(" data-note=\"")?;
            push_escaped_attr(note.as_str(), out)?;
            out.try_push('"')?;
        }
    }
    if let Some(e) = ent {
        let _ = write!(out, " data-ecs-entity=\"{}\"", e.slot());
        if world.has_tag_typed::<SiteHovered>(e).unwrap_or(false) {
            out.try_push_str(" data-ecs-hovered=\"1\"")?;
        }
    }
    out.try_push('>')?;
    Ok(())
}

fn emit_single_word<A: Allocator + Copy>(w: &Word<A>, out: &mut OwnString<A>) -> Result<(), SiteError> {
    if let Some(img) = w.image {
        let cls = match img {
            ImageName::Helix => "ico ico-helix",
            _ if img.is_mask() => "ico ico-mask",
            _ => "ico",
        };
        let h_em = (w.rise + w.drop).max(0.85);
        let va_em = -w.drop;
        let _ = write!(
            out,
            "<span class=\"{}\" style=\"height:{:.3}em;vertical-align:{:.3}em\" aria-hidden=\"true\">",
            cls, h_em, va_em
        );
        out.try_push_str(img.svg_source())?;
        out.try_push_str("</span>")?;
        return Ok(());
    }

    let role_cls = role_css_class(w.role);
    let has_span = role_cls.is_some() || w.raise > 0.0;
    if has_span {
        out.try_push_str("<span class=\"")?;
        if let Some(rc) = role_cls {
            out.try_push_str(rc)?;
            if w.raise > 0.0 {
                out.try_push(' ')?;
            }
        }
        if w.raise > 0.0 {
            out.try_push_str("sup")?;
        }
        out.try_push_str("\">")?;
    }

    if w.scramble.is_empty() {
        push_escaped_text(w.text.as_str(), out)?;
    } else {
        for (k, ch) in w.text.as_str().chars().enumerate() {
            let ku = u32::try_from(k).unwrap_or(u32::MAX);
            let is_scr = w.scramble.as_slice().iter().any(|&s| s == ku);
            if is_scr {
                out.try_push_str("<span class=\"scramble\" data-scramble=\"1\">")?;
                push_escaped_char(ch, out)?;
                out.try_push_str("</span>")?;
            } else {
                push_escaped_char(ch, out)?;
            }
        }
    }

    if has_span {
        out.try_push_str("</span>")?;
    }
    Ok(())
}

fn role_css_class(role: RoleName) -> Option<&'static str> {
    match role {
        RoleName::Cmd => Some("r-cmd"),
        RoleName::CmdDim => Some("r-cmdDim"),
        RoleName::CmdFaint => Some("r-cmdFaint"),
        RoleName::CmdPrompt => Some("r-cmdPrompt"),
        RoleName::Code => Some("r-code"),
        RoleName::CodeArg => Some("r-codeArg"),
        RoleName::CodeProg => Some("r-codeProg"),
        RoleName::CodeMark | RoleName::CodePrompt => Some("r-codeMark"),
        RoleName::RsText => Some("r-rsText"),
        RoleName::RsKw => Some("r-rsKw"),
        RoleName::RsFn => Some("r-rsFn"),
        RoleName::RsTy => Some("r-rsTy"),
        RoleName::RsParam => Some("r-rsParam"),
        RoleName::RsMark => Some("r-rsMark"),
        RoleName::TitleMark | RoleName::SubMark => Some("r-mark"),
        RoleName::TipCode => Some("r-tipCode"),
        RoleName::TipPrompt => Some("r-tipPrompt"),
        _ => None,
    }
}

fn push_escaped_char<A: Allocator>(ch: char, out: &mut OwnString<A>) -> Result<(), SiteError> {
    match ch {
        '&' => out.try_push_str("&amp;")?,
        '<' => out.try_push_str("&lt;")?,
        '>' => out.try_push_str("&gt;")?,
        '"' => out.try_push_str("&quot;")?,
        '\'' => out.try_push_str("&#39;")?,
        other => out.try_push(other)?,
    }
    Ok(())
}

fn push_escaped_text<A: Allocator>(s: &str, out: &mut OwnString<A>) -> Result<(), SiteError> {
    for ch in s.chars() {
        push_escaped_char(ch, out)?;
    }
    Ok(())
}

fn push_escaped_attr<A: Allocator>(s: &str, out: &mut OwnString<A>) -> Result<(), SiteError> {
    for ch in s.chars() {
        push_escaped_char(ch, out)?;
    }
    Ok(())
}
