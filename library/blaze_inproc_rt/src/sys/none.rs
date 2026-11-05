//! Every other platform: nothing can be made, and each constructor says so.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Duration;

use super::Rights;
use crate::channel::Channel;
use crate::handshake::{Grant, Overlay};
use crate::{Access, Error};

fn unsupported<T>() -> Result<T, Error> {
    Err(Error::new(
        "no dual-mapped code memory or start hook here: blaze-inproc runs on Linux x86-64 and AArch64, \
         macOS AArch64 and Windows x86-64",
    ))
}

pub fn page() -> u64 {
    4096
}

pub fn check_icache() -> Result<(), Error> {
    unsupported()
}

pub fn random(_: &mut [u8; 16]) -> Result<(), Error> {
    unsupported()
}

pub enum Object {}

impl Object {
    pub fn create(_: &str, _: u64) -> Result<Object, Error> {
        unsupported()
    }

    pub fn len(&self) -> u64 {
        match *self {}
    }

    pub fn duplicate(&self) -> Result<Object, Error> {
        match *self {}
    }

    pub fn map_here(&self, _: Access) -> Result<View, Error> {
        match *self {}
    }
}

pub enum View {}

impl View {
    pub fn len(&self) -> usize {
        match *self {}
    }

    pub fn address(&self) -> u64 {
        match *self {}
    }

    pub fn bytes(&self, _: Range<usize>) -> Result<&[u8], Error> {
        match *self {}
    }

    pub fn bytes_mut(&mut self, _: Range<usize>) -> Result<&mut [u8], Error> {
        match *self {}
    }

    pub fn word(&self, _: usize) -> &AtomicU64 {
        match *self {}
    }

    pub fn word32(&self, _: usize) -> &AtomicU32 {
        match *self {}
    }

    pub fn publish(&self, _: Range<usize>) {
        match *self {}
    }
}

pub enum Exec {}

impl Exec {
    pub fn new(_: usize, _: usize, _: impl FnOnce(&mut [u8], &mut [u8])) -> Result<Exec, Error> {
        unsupported()
    }

    pub fn code_address(&self) -> u64 {
        match *self {}
    }

    pub fn data_address(&self) -> u64 {
        match *self {}
    }

    pub fn after_fork() {}

    pub fn data(&mut self) -> &mut [u8] {
        match *self {}
    }
}

pub enum Stream {}

impl Stream {
    pub fn read(&mut self, _: &mut [u8]) -> Result<usize, Error> {
        match *self {}
    }

    pub fn write_all(&mut self, _: &[u8]) -> Result<(), Error> {
        match *self {}
    }

    pub fn set_timeout(&mut self, _: Option<Duration>) -> Result<(), Error> {
        match *self {}
    }
}

pub enum Listener {}

impl Listener {
    pub fn bind(_: &Path) -> Result<(Listener, String), Error> {
        unsupported()
    }

    pub fn accept(&self) -> Result<(Stream, u32), Error> {
        match *self {}
    }
}

pub fn connect(_: &str) -> Result<Stream, Error> {
    unsupported()
}

pub enum Remote {}

impl Remote {
    pub fn map(&self, _: u32, _: Access) -> Result<u64, Error> {
        match *self {}
    }

    pub fn overlay(&self, _: u64, _: &Grant) -> Result<(), Error> {
        match *self {}
    }

    pub fn kill(&self) -> Result<(), Error> {
        match *self {}
    }
}

pub enum Bells {}

impl Bells {
    pub fn new(remote: &Remote, _: usize) -> Result<(Bells, Vec<u64>), Error> {
        match *remote {}
    }

    pub fn wake(&self, _: usize, _: &AtomicU64) {
        match *self {}
    }

    pub fn wait_ring(&self, _: &mut Stream) -> Result<bool, Error> {
        match *self {}
    }
}

pub fn ring(stream: &Stream, _: u64) -> Result<(), Error> {
    match *stream {}
}

pub fn hung_up(stream: &Stream) -> bool {
    match *stream {}
}

pub fn wait(_: &AtomicU64, _: u32, _: Duration) -> bool {
    false
}

pub fn wake(_: &AtomicU64) {}

pub fn wait_answer(_: &AtomicU64, _: u32, _: u64, _: Duration) -> bool {
    false
}

pub struct Errno;

impl Errno {
    pub fn keep() -> Errno {
        Errno
    }
}

pub fn send_objects(
    _: &mut Channel,
    _: u32,
    _: &[u8],
    _: &[(&Object, Rights)],
) -> Result<Remote, Error> {
    unsupported()
}

pub enum Granted {}

pub fn recv_objects(_: &mut Channel, _: usize) -> Result<Vec<Granted>, Error> {
    unsupported()
}

impl Granted {
    pub fn len(&self) -> u64 {
        match *self {}
    }

    pub fn map_anywhere(&self, _: &mut Channel, _: Access) -> Result<u64, Error> {
        match *self {}
    }
}

pub fn overlay(_: &mut Channel, _: &[Granted], _: &[Overlay], _: u64) -> Result<(), Error> {
    unsupported()
}

pub struct Scope;

impl Default for Scope {
    fn default() -> Scope {
        Scope::new()
    }
}

impl Scope {
    pub fn new() -> Scope {
        Scope
    }

    pub fn find(&mut self, _: &str) -> Result<Option<u64>, Error> {
        unsupported()
    }
}

pub fn own_image() -> Result<(u64, &'static [u8]), Error> {
    unsupported()
}

pub fn clone_file(_: &Path, _: &Path) -> Result<bool, Error> {
    unsupported()
}

pub fn threads(_: u32, _: &mut Vec<u64>) -> Result<(), Error> {
    unsupported()
}

pub fn service() -> Option<String> {
    None
}

pub fn thread_id() -> u64 {
    0
}

pub fn serialize() {}

pub fn backtrace(_: &mut [u64]) -> Option<usize> {
    None
}
