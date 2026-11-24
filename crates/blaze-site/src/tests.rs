//! Pure `#![no_std]` end-to-end tests for `blaze-site`:
//! - Iterative JSON `PageModel` parsing & relational ECS mounting (`DontFragment` hover tags = 0 moves!)
//! - `selshape` liquid selection row merging, concave inner-corner fillets, and 300ms copy pulse
//! - Responsive column layout, `find_matches`, and copy-text reconstruction
//! - Cursor-anchored trackpad pinch-to-zoom mathematical invariance (`0.25x..=16.0x`)
//! - `BlazeSite` single-pass `den` Slug + SDF instance emission & native WebGPU Cmd+F Find Bar

use blaze_own::String as OwnString;
use blaze_own::testing::heap;
use blaze_ui::slug_shader::site_kind;

use crate::layout::{Viewport, copy_text, find_matches, layout_page, word_range};
use crate::model::{PageModel, SiteHovered};
use crate::palette::ImageName;
use crate::selshape::{Row, join_rows, outline_rows, pulse_scale, shape_of};
use crate::site::{BlazeSite, key};

fn alloc() -> blaze_own::Alloc<'static> {
    heap().alloc()
}

const SAMPLE_PAGE_JSON: &[u8] = br#"{
  "version": 1,
  "table": "/fonts/text.bin",
  "blocks": [
    {
      "kind": "bar",
      "name": [{ "text": "Blaze", "role": "wordmark", "glyphs": [0, 1, 2, 3, 4], "xs": [0.0, 0.6, 0.9, 1.4, 1.9], "adv": 2.4, "space": 0.0 }],
      "backed": [{ "text": "YC", "role": "backed", "glyphs": [0, 1], "xs": [0.0, 0.6], "adv": 1.2, "space": 0.0 }],
      "links": [{ "text": "GitHub", "role": "bar", "glyphs": [0, 1, 2, 3, 4, 5], "xs": [0.0, 0.5, 0.8, 1.1, 1.6, 2.1], "adv": 2.7, "space": 0.0, "link": 0 }]
    },
    {
      "kind": "title",
      "words": [
        { "text": "Blaze", "role": "title", "glyphs": [0, 1, 2, 3, 4], "xs": [0.0, 0.6, 0.9, 1.4, 1.9], "adv": 2.4, "space": 0.3 },
        { "text": "Rust", "role": "title", "glyphs": [0, 1, 2, 3], "xs": [0.0, 0.6, 1.1, 1.5], "adv": 2.0, "space": 0.0 }
      ]
    },
    {
      "kind": "command",
      "words": [
        { "text": "curl", "role": "cmd", "glyphs": [0, 1, 2, 3], "xs": [0.0, 0.6, 1.2, 1.8], "adv": 2.4, "space": 0.6, "part": 0 },
        { "text": "-fsSL", "role": "cmdDim", "glyphs": [0, 1, 2, 3, 4], "xs": [0.0, 0.6, 1.2, 1.8, 2.4], "adv": 3.0, "space": 0.0, "part": 1 }
      ],
      "parts": ["part:curl", "part:flags"],
      "copy": "curl -fsSL https://blazelang.dev/install.sh | sh",
      "button": [{ "text": "Copy", "role": "button", "glyphs": [0, 1, 2, 3], "xs": [0.0, 0.5, 1.0, 1.5], "adv": 2.0, "space": 0.0 }],
      "copied": [{ "text": "Copied", "role": "button", "glyphs": [0, 1, 2, 3, 4, 5], "xs": [0.0, 0.5, 1.0, 1.5, 2.0, 2.5], "adv": 3.0, "space": 0.0 }]
    },
    {
      "kind": "list",
      "items": [
        {
          "words": [
            { "text": "Deterministic", "role": "item", "glyphs": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], "xs": [0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 5.5, 6.0], "adv": 6.5, "space": 0.3 },
            { "text": "Rust", "role": "item", "glyphs": [0, 1, 2, 3], "xs": [0.0, 0.5, 1.0, 1.5], "adv": 2.0, "space": 0.0 }
          ],
          "children": [
            {
              "words": [{ "text": "ReadOnly", "role": "rsTy", "glyphs": [0, 1, 2, 3, 4, 5, 6, 7], "xs": [0.0, 0.6, 1.2, 1.8, 2.4, 3.0, 3.6, 4.2], "adv": 4.8, "space": 0.0, "chip": 1, "link": 1 }]
            }
          ]
        }
      ]
    }
  ],
  "links": [
    { "href": "https://github.com/indexable-inc/blaze" },
    { "href": "", "tip": "ref:ReadOnly" }
  ],
  "tips": {
    "part:flags": {
      "rows": [
        {
          "says": [{ "text": "-f", "role": "tipCode", "glyphs": [0, 1], "xs": [0.0, 0.6], "adv": 1.2, "space": 0.0 }],
          "does": [{ "text": "fail loudly", "role": "tip", "glyphs": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10], "xs": [0.0, 0.4, 0.8, 1.1, 1.4, 1.7, 2.0, 2.4, 2.9, 3.4, 3.8], "adv": 4.3, "space": 0.0 }]
        }
      ]
    },
    "ref:ReadOnly": {
      "title": [{ "text": "ReadOnly", "role": "tipTitle", "glyphs": [0, 1, 2, 3, 4, 5, 6, 7], "xs": [0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5], "adv": 4.0, "space": 0.0 }],
      "body": [{ "text": "Read-only directory capability.", "role": "tip", "glyphs": [0, 1], "xs": [0.0, 0.5], "adv": 8.0, "space": 0.0 }]
    }
  },
  "scramble": { "note": [0, 1], "tip": [0, 1] }
}"#;

#[test]
fn parses_page_model_and_mounts_ecs_with_dont_fragment_tags() {
    let a = alloc();
    let model = PageModel::parse_json(SAMPLE_PAGE_JSON, a).expect("parse sample json");
    assert_eq!(model.blocks.len(), 4);
    assert_eq!(model.links.len(), 2);
    assert_eq!(model.items_arena.len(), 2);
    assert!(model.find_tip("part:flags").is_some());
    assert!(model.find_tip("ref:ReadOnly").is_some());

    let mut site = BlazeSite::new_in(a).expect("new site");
    site.load_page_json(SAMPLE_PAGE_JSON).expect("load json");
    let handles = site.handles.as_ref().expect("ecs handles");
    assert_eq!(handles.link_entities.len(), 2);
    assert_eq!(handles.part_entities.len(), 2);

    // Toggling Flecs 4.2 `DontFragment` tag `SiteHovered` MUST cause zero archetype table migrations,
    // and `blaze_ecs::ecs_query!` filters both `(ChildOf, root)` and `DontFragment` `tag(SiteHovered)`!
    let tables_before = site.world.table_count();
    let root = handles.root;
    let e0 = handles.link_entities.first().copied().expect("link 0");
    site.world.add_tag_typed::<SiteHovered>(e0).expect("add hover");
    assert!(site.world.has_tag_typed::<SiteHovered>(e0).expect("has hover"));

    let mut hovered_links = 0usize;
    blaze_ecs::ecs_query!(&mut site.world, entity, &crate::SiteNodeKind, (blaze_ecs::ChildOf, root), tag(SiteHovered) => |ent, kind| {
        assert_eq!(ent, e0);
        assert_eq!(*kind, crate::SiteNodeKind::Link(0));
        hovered_links += 1;
    })
    .expect("ecs_query hovered");
    assert_eq!(hovered_links, 1);

    let _ = site.world.remove_tag_typed::<SiteHovered>(e0).expect("remove hover");
    assert!(!site.world.has_tag_typed::<SiteHovered>(e0).expect("no hover"));
    assert_eq!(site.world.table_count(), tables_before);
}

#[test]
fn selshape_merges_rows_emits_concave_fillets_and_pulses() {
    let a = alloc();
    let frags = [
        Row { left: 10.0, top: 10.0, right: 120.0, bottom: 30.0 },
        Row { left: 40.0, top: 34.0, right: 160.0, bottom: 54.0 },
    ];
    let joined = join_rows(&frags, 10.0, 1.0, a).expect("join_rows");
    assert_eq!(joined.len(), 2);
    // Neighboring lines snapped together cleanly
    assert!((joined.first().unwrap().bottom - joined.get(1).unwrap().top).abs() < 0.01);

    let (radii, fillets) = outline_rows(joined.as_slice(), 4.0, a).expect("outline_rows");
    assert_eq!(radii.len(), 2);
    // Both left and right edges stepped, producing 2 concave inner-corner fillets!
    assert_eq!(fillets.len(), 2);

    let peak = pulse_scale(0.18);
    assert!(peak > 1.02);
    let ps = shape_of(joined.as_slice(), 50.0, false);
    assert!(ps.live);
    assert!(ps.scale > 1.0);
}

#[test]
fn layout_find_matches_and_cursor_anchored_pinch_zoom_work() {
    let a = alloc();
    let mut site = BlazeSite::new_in(a).expect("site");
    site.load_page_json(SAMPLE_PAGE_JSON).expect("load");
    assert!(site.render_frame(1440.0, 400.0, 2.0, 100.0).is_ok());

    // Find case-insensitively matches both occurrences of "Rust"
    let matches = find_matches(&site.layout, "rust", a).expect("find");
    assert_eq!(matches.len(), 2);
    let mut out = OwnString::new_in(a);
    copy_text(&site.layout, matches.first().unwrap().from, matches.first().unwrap().to, &mut out).expect("copy");
    assert_eq!(out.as_str(), "Rust");

    // Word range selection around first match
    let wr = word_range(&site.layout, matches.first().unwrap().from);
    assert_eq!(wr.from, matches.first().unwrap().from);
    assert_eq!(wr.to, matches.first().unwrap().to);

    // Trackpad pinch-to-zoom keeps the point under cursor (sx, sy) pinned in page coordinates!
    let sx = 680.0;
    let sy = 120.0;
    let (before_px, before_py) = site.screen_to_page(sx, sy);
    assert!(site.on_pinch(sx, sy, 3.0).expect("pinch 3x"));
    assert!((site.zoom() - 3.0).abs() < 1e-4);
    let (after_px, after_py) = site.screen_to_page(sx, sy);
    assert!((before_px - after_px).abs() < 0.05);
    assert!((before_py - after_py).abs() < 0.05);

    // Cmd+F opens native WebGPU find bar and finds "rust"
    assert!(site.on_key_down(key::FIND_F, false, true, 200.0).expect("cmd+f"));
    assert!(site.find_open());
    for ch in "rust".chars() {
        assert!(site.on_text_char(ch, 210.0).expect("type"));
    }
    assert_eq!(site.find_match_count(), 2);

    // Frame emits shapes, find highlights, native find bar, and zoom HUD pill!
    let wants_next = site.render_frame(1440.0, 900.0, 2.0, 220.0).expect("render");
    assert!(wants_next);
    assert!(site.insts.len() > 10);
    assert!(site.insts.as_slice().iter().any(|i| (i.fx.x - site_kind::SHADOW).abs() < 0.1));

    // All 9 SVGs are embedded directly inside `blaze-site` and declare explicit `viewBox`
    for img in ImageName::ALL {
        let src = img.svg_source();
        assert!(src.contains("<svg"));
        assert!(src.contains("viewBox="));
        assert!(!src.contains("<?xml"));
    }

    // Reset zoom returns to 100% centered
    assert!(site.reset_zoom().expect("reset"));
    assert!((site.zoom() - 1.0).abs() < 1e-5);
    assert!((site.pan_x()).abs() < 1e-5);

    // Verify responsive narrow layout builds cleanly too
    let narrow_layout = layout_page(
        &site.model,
        390.0,
        &site.table,
        Viewport { width: 390.0, height: 844.0 },
        a,
    )
    .expect("narrow layout");
    assert!(narrow_layout.height > 100.0);
}

#[test]
fn projects_live_ecs_world_into_semantic_html_for_non_webgpu_browsers() {
    let a = alloc();
    let mut site = BlazeSite::new_in(a).expect("site");
    site.load_page_json(SAMPLE_PAGE_JSON).expect("load");
    let mut html = OwnString::new_in(a);
    let count = site.project_html(&mut html).expect("project_html");
    assert!(count >= 8);

    let s = html.as_str();
    assert!(s.contains("data-ecs-kind=\"page-root\""));
    assert!(s.contains("data-ecs-kind=\"block:bar\""));
    assert!(s.contains("data-ecs-kind=\"block:title\""));
    assert!(s.contains("data-ecs-kind=\"block:command\""));
    assert!(s.contains("data-ecs-kind=\"block:list\""));
    assert!(s.contains("data-ecs-kind=\"list-item\""));
    assert!(s.contains("data-ecs-kind=\"copy-button\""));
    assert!(s.contains("data-copy=\"curl -fsSL https://blazelang.dev/install.sh | sh\""));
    assert!(s.contains("data-tip=\"part:1\""));
    assert!(s.contains("<template data-tip-key=\"ref:ReadOnly\">"));
    assert!(s.contains("ReadOnly"));
    assert!(s.contains("<ul class=\"sub-list\">"));
}

#[test]
fn flecs_style_ecs_query_macro_supports_fetches_filters_relations_dont_fragment_and_cascade() {
    use blaze_ecs::{ChildOf, Entity, Relation, World, ecs_query};
    use blaze_own::Vec as OwnVec;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Pos {
        x: i32,
        y: i32,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Vel {
        dx: i32,
        dy: i32,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Depth(u32);
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Frozen;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Active;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct HoveredSparse;
    struct Likes;
    impl Relation for Likes {}

    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    w.mark_tag_dont_fragment::<HoveredSparse>().expect("df");

    let hero = w.spawn().expect("hero");
    let enemy = w.spawn().expect("enemy");
    let root = w.spawn().expect("root");
    let child = w.spawn().expect("child");
    let grandchild = w.spawn().expect("grandchild");
    let frozen = w.spawn().expect("frozen");
    let prefab = w.spawn().expect("prefab");

    // Hierarchy: root (depth 0) -> child (depth 1) -> grandchild (depth 2)
    w.set_copy(root, Pos { x: 10, y: 0 }).expect("p");
    w.set_copy(root, Vel { dx: 1, dy: 0 }).expect("v");
    w.set_copy(root, Depth(0)).expect("d");
    w.add_tag_typed::<Active>(root).expect("t");

    w.set_copy(child, Pos { x: 20, y: 0 }).expect("p");
    w.set_copy(child, Vel { dx: 2, dy: 0 }).expect("v");
    w.set_copy(child, Depth(1)).expect("d");
    w.add_tag_typed::<Active>(child).expect("t");
    w.relate::<ChildOf>(child, root).expect("r");
    w.relate::<Likes>(child, hero).expect("likes");
    w.add_tag_typed::<HoveredSparse>(child).expect("df hover");

    w.set_copy(grandchild, Pos { x: 30, y: 0 }).expect("p");
    w.set_copy(grandchild, Vel { dx: 3, dy: 0 }).expect("v");
    w.set_copy(grandchild, Depth(2)).expect("d");
    w.add_tag_typed::<Active>(grandchild).expect("t");
    w.relate::<ChildOf>(grandchild, child).expect("r");
    w.relate::<Likes>(grandchild, enemy).expect("likes enemy");

    w.set_copy(frozen, Pos { x: 100, y: 0 }).expect("p");
    w.set_copy(frozen, Vel { dx: 50, dy: 0 }).expect("v");
    w.add_tag_typed::<Active>(frozen).expect("t");
    w.add_tag_typed::<Frozen>(frozen).expect("frozen");

    w.set_copy(prefab, Pos { x: 999, y: 0 }).expect("p");
    w.set_copy(prefab, Vel { dx: 1, dy: 0 }).expect("v");
    w.add_tag(prefab, w.prefab()).expect("prefab");

    // 1. Inline Flecs `each` query with `&mut`, `&`, `!Frozen`, `tag(Active)` (excludes prefab & frozen)
    ecs_query!(&mut w, &mut Pos, &Vel, !Frozen, tag(Active) => |p, v| {
        p.x += v.dx;
    })
    .expect("inline each");
    assert_eq!(w.get::<Pos>(root).unwrap().unwrap().x, 11);
    assert_eq!(w.get::<Pos>(child).unwrap().unwrap().x, 22);
    assert_eq!(w.get::<Pos>(grandchild).unwrap().unwrap().x, 33);
    assert_eq!(w.get::<Pos>(frozen).unwrap().unwrap().x, 100);
    assert_eq!(w.get::<Pos>(prefab).unwrap().unwrap().x, 999);

    // 2. Flecs `cascade(ChildOf)` visits strictly parent-before-child (`0 -> 1 -> 2`)!
    let mut visited_depths: OwnVec<u32, _> = OwnVec::new_in(a);
    ecs_query!(&mut w, &Depth, cascade(ChildOf) => |d| {
        let _ = visited_depths.try_push(d.0);
    })
    .expect("cascade");
    assert_eq!(visited_depths.as_slice(), &[0, 1, 2]);

    // 3. Flecs 4.2 `DontFragment` sparse tag filtering (`tag(HoveredSparse)` & `!tag(HoveredSparse)`)!
    let mut hovered_ents: OwnVec<Entity, _> = OwnVec::new_in(a);
    ecs_query!(&mut w, entity, &Pos, tag(HoveredSparse) => |ent, _p| {
        let _ = hovered_ents.try_push(ent);
    })
    .expect("sparse tag query");
    assert_eq!(hovered_ents.as_slice(), &[child]);

    let not_hovered = ecs_query!(&mut w, &Pos, tag(Active), !Frozen, !tag(HoveredSparse))
        .expect("not hovered")
        .count()
        .expect("count");
    assert_eq!(not_hovered, 2); // root and grandchild

    // 4. Exact & wildcard relation pairs: `(Likes, *)`, `!(Likes, enemy)`, `(ChildOf, root)`!
    let likes_any = ecs_query!(&mut w, &Pos, (Likes, *)).expect("q").count().expect("c");
    assert_eq!(likes_any, 2);
    let likes_not_enemy = ecs_query!(&mut w, &Pos, (Likes, *), !(Likes, enemy))
        .expect("q")
        .count()
        .expect("c");
    assert_eq!(likes_not_enemy, 1);

    // 5. SoA `chunk` mode & `prefabs` inclusion + `.build_cached()`
    let mut chunk_sum = 0i32;
    ecs_query!(&mut w, chunk, &Pos, (ChildOf, root) => |ps| {
        for p in ps {
            chunk_sum += p.x;
        }
    })
    .expect("chunk");
    assert_eq!(chunk_sum, 22);

    let with_prefabs = ecs_query!(&mut w, &Pos, prefabs).expect("prefabs").count().expect("c");
    assert_eq!(with_prefabs, 5);

    let mut cached = ecs_query!(&mut w, &Pos, &Vel, !Frozen)
        .expect("build")
        .build_cached()
        .expect("cached");
    let matched_tables = w.query_tables(&mut cached).expect("sync");
    assert!(!matched_tables.is_empty());
}

#[test]
fn renders_ecs_driven_open_graph_card_via_rust_gpu_slug_shader() {
    use blaze_own::Vec as OwnVec;

    let a = alloc();
    let mut site = BlazeSite::new_in(a).expect("site");
    site.load_page_json(SAMPLE_PAGE_JSON).expect("load");
    let mut rgba: OwnVec<u8, _> = OwnVec::new_in(a);
    let stats = site.render_og_rgba(&mut rgba).expect("render_og_rgba");
    assert_eq!(stats.width, crate::OG_WIDTH);
    assert_eq!(stats.height, crate::OG_HEIGHT);
    assert_eq!(rgba.len(), 1200 * 630 * 4);
    assert!(stats.ecs_entities_queried >= 4);
    assert!(stats.inst_count >= 6);
    // Verify the obsidian border card / ember accent actually painted non-uniform pixels
    let first_px = rgba.as_slice().get(0..4).unwrap_or(&[]);
    let mut diff_pixels = 0usize;
    for chunk in rgba.as_slice().chunks_exact(4) {
        if chunk != first_px {
            diff_pixels += 1;
        }
    }
    assert!(stats.view_entities_laid_out >= 10);
    assert!(diff_pixels > 1000, "expected rendered SDF plates/rings/accent on OG card");
}

#[test]
fn flecs_script_ecs_scene_macro_declares_prefabs_relations_tags_singletons_and_hierarchies() {
    use blaze_ecs::{ChildOf, Relation, World, ecs_query, ecs_scene};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Mass(u32);
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Radius(u32);
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Gravity(u32);
    struct Habitable;
    struct Orbits;
    impl Relation for Orbits {}

    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    ecs_scene!(&mut w, {
        singleton Gravity = Gravity(98);

        prefab Planet {
            copy Mass = Mass(100);
            copy Radius = Radius(64);
        }

        SolarSystem {
            let earth = Earth : Planet {
                #Habitable;
                copy Radius = Radius(6371);
                Moon {
                    (Orbits, earth);
                    copy Mass = Mass(1);
                }
            }
        }
    })
    .expect("ecs_scene");

    assert_eq!(w.singleton::<Gravity>().unwrap().copied(), Some(Gravity(98)));
    let sys = w.entity_by_name("SolarSystem").expect("SolarSystem");
    let earth = w.entity_by_name("Earth").expect("Earth");
    let moon = w.entity_by_name("Moon").expect("Moon");
    assert_eq!(w.parent_of(earth).unwrap(), Some(sys));
    assert_eq!(w.parent_of(moon).unwrap(), Some(earth));
    assert!(w.has_tag_typed::<Habitable>(earth).unwrap());
    assert!(w.has_relation::<Orbits>(moon, earth).unwrap());
    assert_eq!(w.get::<Mass>(earth).unwrap().copied(), Some(Mass(100)));
    assert_eq!(w.get::<Radius>(earth).unwrap().copied(), Some(Radius(6371)));

    let orbiters = ecs_query!(&mut w, &Mass, (Orbits, earth), (ChildOf, earth))
        .expect("q")
        .count()
        .expect("c");
    assert_eq!(orbiters, 1);
}
