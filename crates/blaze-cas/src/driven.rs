//! A store opened and driven on the calling thread by this target's native driver, for what runs
//! without the daemon: a tool, a benchmark, a test. The daemon drives its store in its own loop.

use std::path::{Path, PathBuf};

use blaze_core::{Effect, FileName, Native, run_until_idle};

use crate::{Config, Handle, Store};

/// The tags its store asks with; nothing else asks this driver.
const TAGS: u64 = 1 << 48;

pub struct Driven {
    driver: Native,
    store: Store,
    root: PathBuf,
}

impl Driven {
    /// The store in `root/store`, made if it is missing, open, under a native driver whose root
    /// is `root`.
    pub fn open(root: &Path) -> Result<Driven, String> {
        let dir = FileName::new("store").expect("a relative name");
        let at = root.join(dir.as_str());
        let mut driver = blaze_core::native(root, 2)
            // alloc: the error of a store that cannot open
            .map_err(|e| format!("the native driver over {}: {e}", root.display()))?;
        // alloc: setup, once a store
        let mut effects = Vec::new();
        // alloc: setup, no layers: allocates nothing
        let mut store = Store::open(Config { dir, layers: Vec::new(), tags: TAGS }, &mut effects);
        drive(&mut driver, &mut store, &mut effects);
        if let Some(broken) = store.broken() {
            // alloc: the error of a store that cannot open
            return Err(broken.to_string());
        }
        if !store.ready() {
            // alloc: the error of a store that cannot open
            return Err(format!("the store in {} did not open: its driver went idle first", at.display()));
        }
        // alloc: setup, once a store
        Ok(Driven { driver, store, root: root.to_owned() })
    }

    pub fn store(&mut self) -> &mut Store {
        &mut self.store
    }

    /// A handle on it, for code that reads and puts through one; what it puts is durable at the
    /// next [`Driven::flush`].
    pub fn handle(&mut self) -> Handle {
        // clone: setup, once a handle
        Handle::new(&mut self.store, self.root.clone())
    }

    /// What it was given (by it and every writer and handle of it), durable: a commit, driven
    /// until it is done.
    pub fn flush(&mut self) -> Result<(), String> {
        // alloc: once a flush, which a tool asks for once
        let mut effects = Vec::new();
        self.store.flush(&mut effects);
        drive(&mut self.driver, &mut self.store, &mut effects);
        match self.store.broken() {
            // alloc: the error of a store that stopped
            Some(broken) => Err(broken.to_string()),
            None => Ok(()),
        }
    }
}

fn drive(driver: &mut Native, store: &mut Store, effects: &mut Vec<Effect>) {
    run_until_idle(driver, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    });
}
