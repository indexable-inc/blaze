use std::{collections::VecDeque, hash::Hash};

use blaze_hash::HashSet;

/// A worklist with set semantics: `push` of a queued item is a no-op; `pop` is FIFO. An item
/// popped may be pushed again. The ring and the set keep their buffers, so a warm worklist
/// allocates nothing.
#[derive(Debug)]
pub struct Worklist<T> {
    queue: VecDeque<T>,
    queued: HashSet<T>,
}

impl<T: Copy + Eq + Hash> Default for Worklist<T> {
    fn default() -> Self {
        Worklist::new()
    }
}

impl<T: Copy + Eq + Hash> Worklist<T> {
    pub fn new() -> Self {
        Worklist { queue: VecDeque::new(), queued: HashSet::default() }
    }

    /// Queues `t` unless it is queued; whether it was not.
    pub fn push(&mut self, t: T) -> bool {
        let new = self.queued.insert(t);
        if new {
            self.queue.push_back(t);
        }
        new
    }

    pub fn pop(&mut self) -> Option<T> {
        let t = self.queue.pop_front()?;
        self.queued.remove(&t);
        Some(t)
    }

    pub fn contains(&self, t: T) -> bool {
        self.queued.contains(&t)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.queued.clear();
    }
}
