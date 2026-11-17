//! The blaze site's renderer: a world (the site's content as Flecs entities) in, one page's HTML out. Every page is
//! static: the build renders each once. Sans-IO: no files, no clock and no state outside the `Site` value, so it runs
//! natively or in any wasm host. README.md has the architecture.
//!
//! - `world`: the entity store the renderer reads.
//! - `snapshot`: a world from a flecs-snapshot DAG-CBOR document, which is what hosts load.
//! - `scripts` (feature `scripts`, native): the content scripts run in Flecs and snapshotted.
//! - `render`, `inline`: the pages and their blocks, and inline text.
//! - `chart`: the charts (`RowChart`, `LineChart`) as HTML rows and inline SVG, and the `Picker`'s buttons.
//! - `course`: the course path as a graph of lessons and updates, laid out in rows, columns and bands.
//! - `side`, `icons`: the sidebar every page carries (sections, rows, the pager, the outline) and its icons.

pub mod chart;
pub mod course;
pub mod icons;
pub mod inline;
pub mod js;
pub mod render;
#[cfg(feature = "scripts")]
pub mod scripts;
pub mod side;
pub mod snapshot;
pub mod text;
pub mod world;

pub use render::{Page, Served};
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
}

impl Site {
    pub fn new(world: World) -> Result<Site, String> {
        let site = world.lookup("site").ok_or("the world has no `site` entity")?;
        let pages = render::pages(&world, site);
        let nav = side::build(&world, &pages)?;
        let refs = world.lookup("refs");
        let served = Served::of(&world, site)?;
        Ok(Site { world, site, pages, nav, refs, served })
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

    /// The page at `url`: its whole HTML.
    pub fn render(&self, url: &str) -> Result<String, String> {
        let page = self.page(url).ok_or_else(|| format!("no page at {url}"))?;
        let s = render::Site {
            world: &self.world,
            site: self.site,
            pages: &self.pages,
            nav: &self.nav,
            refs: self.refs,
            served: &self.served,
        };
        render::render(&s, page)
    }
}
