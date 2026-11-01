//! The images of a machine: one per key, opened once (parsed, checked, written into its
//! memory object) and shared by every view and run, never one per view. A build tries the
//! newest image of its dependency graph first, before it has a key of its own: when every
//! piece the image has is the build's, the dependency instances it lacks are linked with the
//! workspace while the image of the build's key is made in the background. Only a build the
//! newest image does not serve keys its own and waits for it; an image is made again only
//! when a build needs what its key adds.

use std::sync::Arc;

use blaze_hash::{HashMap, HashSet};

use crate::PremapError;
use crate::image::View;
use crate::key::{GraphKey, ImageKey};
use crate::map::Memory;

/// Dependency graphs kept at once: a machine's branches and toolchains, not its history.
pub const GRAPHS: usize = 8;

/// An opened image: its memory object and its metadata, not its segment bytes again.
pub struct Ready {
    pub key: ImageKey,
    pub graph: GraphKey,
    meta: Box<[u8]>,
    memory: Memory,
}

impl Ready {
    /// Checks `bytes` as the image of `key` and fills its memory object.
    pub fn open(graph: GraphKey, key: ImageKey, bytes: &[u8]) -> Result<Ready, PremapError> {
        let view = View::parse(bytes, key)?;
        let memory = Memory::of(&view)?;
        Ok(Ready { key, graph, meta: bytes[..view.meta_len()].into(), memory })
    }

    /// The image's metadata: its segments' bytes are in [`Ready::memory`].
    pub fn view(&self) -> View<'_> {
        View::reopen(&self.meta, self.key)
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }
}

/// What a build whose graph's newest image is not its own links against.
pub enum Pick {
    /// The image of the build's key.
    Use(Arc<Ready>),
    /// The build's key is being made: wait for it.
    Wait,
    /// The build's key is not open or being made: make it, after looking in the store,
    /// having claimed it ([`Images::finish`] gives it up).
    Make,
    /// Making the build's key failed: deterministic, so it fails again.
    Failed(PremapError),
}

#[derive(Default)]
pub struct Images {
    open: HashMap<ImageKey, Arc<Ready>>,
    newest: HashMap<GraphKey, ImageKey>,
    making: HashSet<ImageKey>,
    failed: HashMap<ImageKey, PremapError>,
    used: HashMap<GraphKey, u64>,
    clock: u64,
}

impl Images {
    /// The newest image of `graph`, which a build tries first, with no key of its own yet;
    /// marks the graph used.
    pub fn newest(&mut self, graph: GraphKey) -> Option<Arc<Ready>> {
        self.clock += 1;
        self.used.insert(graph, self.clock);
        self.newest.get(&graph).map(|key| Arc::clone(&self.open[key]))
    }

    /// The image of `key`, for a build its graph's newest image does not serve.
    pub fn want(&mut self, key: ImageKey) -> Pick {
        if let Some(ready) = self.open.get(&key) {
            return Pick::Use(Arc::clone(ready));
        }
        if let Some(error) = self.failed.get(&key) {
            return Pick::Failed(error.clone()); // clone: error: a failed key fails again with its error
        }
        if self.making.insert(key) { Pick::Make } else { Pick::Wait }
    }

    /// Claims making `key` in the background, for a build that used an older image of its
    /// graph and spilled: `false` when it is open or being made already.
    pub fn claim(&mut self, key: ImageKey) -> bool {
        !self.open.contains_key(&key) && !self.failed.contains_key(&key) && self.making.insert(key)
    }

    /// The end of making `key`, claimed by [`Images::want`] or [`Images::claim`]: it becomes
    /// its graph's newest image, and the image it replaces is closed once no run maps it.
    pub fn finish(&mut self, key: ImageKey, made: Result<Ready, PremapError>) -> Result<Arc<Ready>, PremapError> {
        assert!(self.making.remove(&key), "finishing a dependency image nothing claimed");
        let ready = match made {
            Ok(ready) => Arc::new(ready), // alloc: setup, once per image opened
            Err(error) => {
                self.failed.insert(key, error.clone()); // clone: error: kept for the builds after
                return Err(error);
            }
        };
        assert!(ready.key == key, "finishing a dependency image under another key");
        if let Some(old) = self.newest.insert(ready.graph, key) {
            self.open.remove(&old);
        }
        self.open.insert(key, Arc::clone(&ready));
        self.used.entry(ready.graph).or_insert(self.clock);
        while self.newest.len() > GRAPHS {
            let (&graph, _) = self
                .used
                .iter()
                .filter(|(graph, _)| self.newest.contains_key(graph))
                .min_by_key(|(_, at)| **at)
                .expect("more graphs than none");
            let key = self.newest.remove(&graph).expect("a graph with an image");
            self.open.remove(&key);
            self.used.remove(&graph);
        }
        Ok(ready)
    }

    pub fn get(&self, key: &ImageKey) -> Option<Arc<Ready>> {
        self.open.get(key).cloned()
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}
