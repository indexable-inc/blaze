//! The store as the kernel reads and writes it.

use blaze_cas::{Guard, Key, Kind, Mismatch, Reader, Store, Writer};

/// Reads values, verified.
pub trait Get {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String>;

    /// Whether it has `key`. Lookup probes with this, then reads (verified) only what it takes.
    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.get(key)?.is_some())
    }
}

/// Keeps values. A put is readable through its own [`Get`] at once. A key it already has with
/// other bytes is a [`Mismatch`], and it keeps the first.
pub trait Put: Get {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch>;
}

impl Get for Store {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(Store::get(self, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(Store::has(self, key))
    }
}

impl Put for Store {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        Store::put(self, key, kind, &bytes).map(drop)
    }
}

/// A store read from a task: a [`Reader`] under the task's guard.
pub struct Reading<'g> {
    pub reader: &'g Reader,
    pub guard: &'g Guard,
}

impl Get for Reading<'_> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(self.reader.get(self.guard, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.reader.has(self.guard, key))
    }
}

/// A store read and written from a task: a worker's [`Writer`], whose puts its [`Reader`]
/// reads at once.
pub struct Writing<'g> {
    pub reader: &'g Reader,
    pub writer: &'g mut Writer,
    pub guard: &'g Guard,
}

impl Get for Writing<'_> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(self.reader.get(self.guard, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.reader.has(self.guard, key))
    }
}

impl Put for Writing<'_> {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        self.writer.put(self.guard, key, kind, &bytes).map(drop)
    }
}
