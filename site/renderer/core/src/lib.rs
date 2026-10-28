//! The blaze site's renderer: a world (the site's content as Flecs entities, comments as entities under their
//! blocks) and a viewer in, one page's HTML out, byte for byte what the TS renderer in site/render/ writes. Sans-IO:
//! no files, no clock and no state outside the `Site` value, so it runs at build time natively and per request in any
//! wasm host. README.md has the architecture.
//!
//! - `world`: the entity store the renderer reads.
//! - `snapshot`: a world from a flecs-snapshot DAG-CBOR document, which is what hosts load.
//! - `scripts` (feature `scripts`, native): the content scripts run in Flecs and snapshotted.
//! - `render`, `inline`, `comments`: render/render.ts, widgets.ts, inline.ts and comments.ts.
//! - `chart`: the charts (`RowChart`, `LineChart`) as HTML rows and inline SVG, and the `Picker`'s buttons.
//! - `course`: the course path as a graph of lessons and updates, laid out in rows, columns and bands.
//! - `side`, `icons`: the sidebar every page carries (sections, rows, the pager, the outline) and its icons.
//! - `spoken`: a page's spoken version (the narration's lines), derived from its blocks or written with `Say`.

pub mod chart;
pub mod comments;
pub mod course;
pub mod icons;
pub mod inline;
pub mod js;
pub mod render;
#[cfg(feature = "scripts")]
pub mod scripts;
pub mod side;
pub mod snapshot;
pub mod spoken;
pub mod text;
pub mod world;

pub use comments::{Comment, Discussion, FormError, Viewer};
pub use render::{Blocks, Page, Served};
pub use world::{Entity, Id, Scalar, Value, World};

/// A site ready to render: the world and the indexes built once per isolate (pages, the sidebar, refs).
pub struct Site {
    world: World,
    site: Id,
    pages: Vec<Page>,
    /// The sidebar's sections (`side.rs`).
    nav: Vec<side::Group>,
    refs: Option<Id>,
    served: Served,
    /// Per discussable page (by URL): its blocks from a comment-less render, which says where comments hang.
    threads: std::collections::BTreeMap<String, Blocks>,
}

/// One rendered page.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendered {
    pub html: String,
    /// For a discussable page: the blocks comments may attach to, and their text (`.gen/discussable.json`).
    pub blocks: Option<Blocks>,
}

impl Site {
    pub fn new(world: World) -> Result<Site, String> {
        let site = world.lookup("site").ok_or("the world has no `site` entity")?;
        let pages = render::pages(&world, site);
        let nav = side::build(&world, &pages)?;
        let refs = world.lookup("refs");
        let served = Served::of(&world, site)?;
        let mut site = Site { world, site, pages, nav, refs, served, threads: Default::default() };
        for p in site.pages.clone() {
            if site.is_discussable(&p)
                && let Some(b) = site.render(&p.url, &[], &Discussion::default())?.blocks
            {
                site.threads.insert(p.url.clone(), b);
            }
        }
        Ok(site)
    }

    /// Each discussable page's blocks (`.gen/discussable.json`), in page order.
    pub fn discussable(&self) -> Vec<&Blocks> {
        self.pages.iter().filter_map(|p| self.threads.get(&p.url)).collect()
    }

    /// The site in a flecs-snapshot document (what the build writes to `site.cbor`).
    pub fn from_snapshot(bytes: &[u8]) -> Result<Site, String> {
        Site::new(snapshot::load(bytes)?)
    }

    /// Where the site is served (`Served` on `site`).
    pub fn served(&self) -> &Served {
        &self.served
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    pub fn page(&self, url: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.url == url)
    }

    /// The page takes comments: block by block (`Discussable`), or on the page as a whole (every page not `Quiet`).
    pub fn is_discussable(&self, page: &Page) -> bool {
        crate::render::talks(self.world.get(page.node))
    }

    /// The spoken lines of the page at `url` (`spoken.rs`), in order.
    pub fn spoken(&self, url: &str) -> Result<Vec<spoken::Line>, String> {
        let page = self.page(url).ok_or_else(|| format!("no page at {url}"))?;
        let s = render::Site {
            world: &self.world,
            site: self.site,
            pages: &self.pages,
            nav: &self.nav,
            refs: self.refs,
            served: &self.served,
        };
        spoken::lines(&s, page)
    }

    /// The page at `url` with `comments` spawned under their blocks for this render only, for `discussion`'s viewer.
    pub fn render(
        &mut self,
        url: &str,
        comments: &[Comment],
        discussion: &Discussion,
    ) -> Result<Rendered, String> {
        let page = self.page(url).ok_or_else(|| format!("no page at {url}"))?.clone();
        let mark = self.world.mark();
        let threads = self.threads.get(url).map(|b| b.blocks.as_slice()).unwrap_or(&[]);
        let out = comments::spawn(&mut self.world, page.node, threads, comments).and_then(|()| {
            let s = render::Site {
                world: &self.world,
                site: self.site,
                pages: &self.pages,
                nav: &self.nav,
                refs: self.refs,
                served: &self.served,
            };
            render::render(&s, &page, discussion)
        });
        self.world.rollback(mark);
        let (html, blocks) = out?;
        Ok(Rendered { html, blocks })
    }

    /// `render` with its inputs as a host sends them, in plain CBOR (any key order, any encoder): `comments` a list of
    /// `{id, block, quote, body, created_at, user_id, login, name, avatar_url}`, `discussion` a map
    /// `{viewer: null | {id, login, admin}, sign_in, error: null | {block, message}}`. Empty bytes mean none.
    pub fn render_cbor(
        &mut self,
        url: &str,
        comments: &[u8],
        discussion: &[u8],
    ) -> Result<Rendered, String> {
        let comments: Vec<Comment> = if comments.is_empty() {
            Vec::new()
        } else {
            ciborium::from_reader(comments).map_err(|e| format!("comments: {e}"))?
        };
        let discussion: Discussion = if discussion.is_empty() {
            Discussion::default()
        } else {
            ciborium::from_reader(discussion).map_err(|e| format!("discussion: {e}"))?
        };
        self.render(url, &comments, &discussion)
    }
}
