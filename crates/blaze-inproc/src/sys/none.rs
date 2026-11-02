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

pub(crate) fn page() -> u64 {
    4096
}

pub(crate) fn check_icache() -> Result<(), Error> {
    unsupported()
}

pub(crate) fn random(_: &mut [u8; 16]) -> Result<(), Error> {
    unsupported()
}

pub(crate) enum Object {}

impl Object {
    pub(crate) fn create(_: &str, _: u64) -> Result<Object, Error> {
        unsupported()
    }

    pub(crate) fn len(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        match *self {}
    }

    pub(crate) fn map_here(&self, _: Access) -> Result<View, Error> {
        match *self {}
    }
}

pub(crate) enum View {}

impl View {
    pub(crate) fn len(&self) -> usize {
        match *self {}
    }

    pub(crate) fn address(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn bytes(&self, _: Range<usize>) -> Result<&[u8], Error> {
        match *self {}
    }

    pub(crate) fn bytes_mut(&mut self, _: Range<usize>) -> Result<&mut [u8], Error> {
        match *self {}
    }

    pub(crate) fn word(&self, _: usize) -> &AtomicU64 {
        match *self {}
    }

    pub(crate) fn word32(&self, _: usize) -> &AtomicU32 {
        match *self {}
    }

    pub(crate) fn publish(&self, _: Range<usize>) {
        match *self {}
    }
}

pub(crate) enum Exec {}

impl Exec {
    pub(crate) fn new(_: usize, _: usize, _: impl FnOnce(&mut [u8], &mut [u8])) -> Result<Exec, Error> {
        unsupported()
    }

    pub(crate) fn code_address(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn data_address(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn data(&mut self) -> &mut [u8] {
        match *self {}
    }
}

pub(crate) enum Stream {}

impl Stream {
    pub(crate) fn read(&mut self, _: &mut [u8]) -> Result<usize, Error> {
        match *self {}
    }

    pub(crate) fn write_all(&mut self, _: &[u8]) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn set_timeout(&mut self, _: Option<Duration>) -> Result<(), Error> {
        match *self {}
    }
}

pub(crate) enum Listener {}

impl Listener {
    pub(crate) fn bind(_: &Path) -> Result<(Listener, String), Error> {
        unsupported()
    }

    pub(crate) fn accept(&self) -> Result<(Stream, u32), Error> {
        match *self {}
    }
}

pub(crate) fn connect(_: &str) -> Result<Stream, Error> {
    unsupported()
}

pub(crate) enum Remote {}

impl Remote {
    pub(crate) fn map(&self, _: u32, _: Access) -> Result<u64, Error> {
        match *self {}
    }

    pub(crate) fn overlay(&self, _: u64, _: &Grant) -> Result<(), Error> {
        match *self {}
    }

    pub(crate) fn kill(&self) -> Result<(), Error> {
        match *self {}
    }
}

pub(crate) enum Bells {}

impl Bells {
    pub(crate) fn new(remote: &Remote, _: usize) -> Result<(Bells, Vec<u64>), Error> {
        match *remote {}
    }

    pub(crate) fn wake(&self, _: usize, _: &AtomicU64) {
        match *self {}
    }

    pub(crate) fn wait_ring(&self, _: &mut Stream) -> Result<bool, Error> {
        match *self {}
    }
}

pub(crate) fn ring(stream: &Stream, _: u64) -> Result<(), Error> {
    match *stream {}
}

pub(crate) fn hung_up(stream: &Stream) -> bool {
    match *stream {}
}

pub(crate) fn wait(_: &AtomicU64, _: u32, _: Duration) -> bool {
    false
}

pub(crate) fn wake(_: &AtomicU64) {}

pub(crate) fn wait_answer(_: &AtomicU64, _: u32, _: u64, _: Duration) -> bool {
    false
}

pub(crate) struct Errno;

impl Errno {
    pub(crate) fn keep() -> Errno {
        Errno
    }
}

pub(crate) fn send_objects(_: &mut Channel, _: u32, _: &[u8], _: &[(&Object, Rights)]) -> Result<Remote, Error> {
    unsupported()
}

pub(crate) enum Granted {}

pub(crate) fn recv_objects(_: &mut Channel, _: usize) -> Result<Vec<Granted>, Error> {
    unsupported()
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn map_anywhere(&self, _: &mut Channel, _: Access) -> Result<u64, Error> {
        match *self {}
    }
}

pub(crate) fn overlay(_: &mut Channel, _: &[Granted], _: &[Overlay], _: u64) -> Result<(), Error> {
    unsupported()
}

pub(crate) struct Scope;

impl Scope {
    pub(crate) fn new() -> Scope {
        Scope
    }

    pub(crate) fn find(&mut self, _: &str) -> Result<Option<u64>, Error> {
        unsupported()
    }
}

pub(crate) fn own_image() -> Result<(u64, &'static [u8]), Error> {
    unsupported()
}

pub(crate) fn clone_file(_: &Path, _: &Path) -> Result<bool, Error> {
    unsupported()
}

pub(crate) fn threads(_: u32, _: &mut Vec<u64>) -> Result<(), Error> {
    unsupported()
}

pub(crate) fn service() -> Option<String> {
    None
}

pub(crate) fn thread_id() -> u64 {
    0
}

pub(crate) fn serialize() {}

pub(crate) fn backtrace(_: &mut [u64]) -> Option<usize> {
    None
}
