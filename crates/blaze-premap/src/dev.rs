//! A dev build's way to its dependency image: [`plan`] keys the build's graph and checks the
//! graph's newest image of the machine's [`Images`] against the build; only when that image
//! does not serve it does it split off the build's dependency pieces and key their image. A
//! [`Job`] makes an image (from the store when it is there, else by the prelink) for
//! [`Images::finish`].
//!
//! The daemon's dev run (`v2/requests/fastcompile.md`, fastcompile-premap 5):
//!
//! 1. [`plan`]. On [`Plan::Make`], run the job now (the graph has no image yet), finish it,
//!    plan again; on [`Plan::Wait`], wait for the build making it.
//! 2. On [`Plan::Link`], link the workspace executable beside the image (its `View` is the
//!    link's outside), and run it with the image's fixed maps and the cells
//!    [`crate::start::plan`] binds; run `next`, when set, at idle priority and finish it, so
//!    the next build links against it.

use std::sync::Arc;

use blaze_ids::ContentId;

use crate::PremapError;
use crate::cache::{Images, Pick, Ready};
use crate::key::{GraphInputs, GraphKey, ImageInputs, ImageKey};
use crate::prelink::{self, Prelink, Request};
use crate::split::{Built, Fit, ImagePart, Owned, SidedNative, WorkspacePart, image_part, workspace_part};
use crate::store::Shelf;

/// A dev build, as premap takes it.
pub struct Dev<'a> {
    pub graph: GraphInputs<'a>,
    pub built: Built<'a>,
    pub owned: Owned<'a>,
    pub natives: &'a [SidedNative<'a>],
    /// The span tables the graph's span keys name, as the linker fills positions from.
    pub spans: &'a [(ContentId, &'a [u8])],
}

pub enum Plan<'a> {
    /// Link `workspace` against `image`. `next` is set when the image lacks dependency
    /// pieces of this build: the image of its key, claimed, to make in the background.
    Link { image: Arc<Ready>, workspace: WorkspacePart<'a>, next: Option<Job<'a>> },
    /// No image serves the build: make the image of its key, claimed, and plan again.
    Make(Job<'a>),
    /// Another build is making the image of this one's key: wait for it and plan again.
    Wait,
}

/// An image to make, claimed in [`Images`].
pub struct Job<'a> {
    pub graph: GraphKey,
    pub key: ImageKey,
    part: ImagePart<'a>,
    dev: &'a Dev<'a>,
}

/// A build its graph's newest image serves keys nothing but its graph: the image's pieces are
/// checked against the build's ([`workspace_part`]). Only a build it does not serve splits
/// off and keys its own image.
pub fn plan<'a>(images: &mut Images, dev: &'a Dev<'a>) -> Result<Plan<'a>, PremapError> {
    let graph = GraphKey::of(&dev.graph);
    let newest = images.newest(graph);
    if let Some(image) = &newest
        && let Fit::Part(workspace) = workspace_part(&dev.built, &dev.owned, &image.view(), dev.natives)?
    {
        let image = Arc::clone(image);
        if workspace.complete() {
            return Ok(Plan::Link { image, workspace, next: None });
        }
        let (key, part) = own_key(graph, dev)?;
        if image.key == key {
            return Err(PremapError::of(format_args!(
                "the dependency image {:?} lacks {} functions and {} statics its own pieces have",
                key.0,
                workspace.spilled.len(),
                workspace.spilled_statics.len()
            )));
        }
        let next = images.claim(key).then_some(Job { graph, key, part, dev });
        return Ok(Plan::Link { image, workspace, next });
    }
    let (key, part) = own_key(graph, dev)?;
    match images.want(key) {
        Pick::Use(image) => match workspace_part(&dev.built, &dev.owned, &image.view(), dev.natives)? {
            Fit::Part(workspace) if workspace.complete() => Ok(Plan::Link { image, workspace, next: None }),
            Fit::Part(workspace) => Err(PremapError::of(format_args!(
                "the dependency image {:?} lacks {} functions and {} statics its own pieces have",
                key.0,
                workspace.spilled.len(),
                workspace.spilled_statics.len()
            ))),
            Fit::Stale(stale) => {
                Err(PremapError::of(format_args!("the dependency image {:?} is not of its own pieces: {stale:?}", key.0)))
            }
        },
        Pick::Make => Ok(Plan::Make(Job { graph, key, part, dev })),
        Pick::Wait => Ok(Plan::Wait),
        Pick::Failed(error) => Err(error),
    }
}

/// The dependency pieces of `dev` and the key of their image.
fn own_key<'a>(graph: GraphKey, dev: &'a Dev<'a>) -> Result<(ImageKey, ImagePart<'a>), PremapError> {
    let part = image_part(&dev.built, &dev.owned, dev.natives)?;
    let key = ImageKey::of(
        graph,
        &ImageInputs {
            functions: &part.keyed_functions,
            statics: &part.keyed_statics,
            names: &part.names,
            debug_names: &part.debug_names,
        },
    );
    Ok((key, part))
}

impl Job<'_> {
    /// The image, from `shelf` when it keeps it, else linked by `linker` and kept there.
    pub fn make(&self, linker: &impl Prelink, shelf: &mut impl Shelf) -> Result<Ready, PremapError> {
        if let Some(bytes) = shelf.get_image(&self.key)? {
            return Ready::open(self.graph, self.key, bytes);
        }
        let name = format!("blaze-premap-{}", self.key.0); // alloc: setup, once per image made: its output's name
        let output = prelink::output(self.dev.graph.output, &name);
        let request = Request {
            link: blaze_link::premap::Request {
                output: &output,
                base: self.dev.graph.base,
                tls_bias: self.dev.graph.tls_bias,
                functions: &self.part.functions,
                statics: &self.part.statics,
                names: &self.part.names,
                debug_names: &self.part.debug_names,
                natives: &self.part.natives,
                spans: self.dev.spans,
            },
            function_digests: &self.part.function_digests,
            static_digests: &self.part.static_digests,
            debug_digests: &self.part.debug_digests,
        };
        let bytes = prelink::build(linker, &request, self.key)?;
        let ready = Ready::open(self.graph, self.key, &bytes)?;
        shelf.put_image(&self.key, bytes)?;
        Ok(ready)
    }
}
