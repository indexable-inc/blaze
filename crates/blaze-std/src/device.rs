//! The device capabilities (docs/v2/plan/design/std.md, "Sound, and the device capabilities"):
//! [`Camera`], [`Screen`], [`Location`], [`Bluetooth`], [`Usb`], [`Serial`], [`Clipboard`] and
//! [`Notify`]. Each has a type and its narrowing, so that code cannot reach a device unasked
//! and a host can say it has none, and nothing more: a device's protocol logic lives outside
//! bstd as a sans-IO core over the capability.
//!
//! | capability | narrowed to | why it is authority |
//! |---|---|---|
//! | [`Camera`] | a device | watches |
//! | [`Screen`] | a display or a window | watches |
//! | [`Location`] | a precision | says where the user is |
//! | [`Bluetooth`] | a device, a service | scanning reveals location; pairing reaches other devices |
//! | [`Usb`], [`Serial`] | a device | reaches hardware directly |
//! | [`Clipboard`] | read or write | holds whatever the user last copied |
//! | [`Notify`] | | interrupts the user |
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::devices`] and beside it): each
//! has just the operations one test needs, a scripted value in and a recorded value out.
//! blaze-core has no effect for any of them.

use alloc::{borrow::ToOwned, vec::Vec};
use core::fmt;

use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    cx::Cx,
    hosted::{self, Refused, SimHost},
    io::Io,
    mem::Bytes,
    name::{DeviceName, ServiceName},
    narrow::Allowed,
    window::WindowId,
};

/// Which kind of device a simulated one is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    Camera,
    Display,
    Bluetooth,
    Usb,
    Serial,
}

/// A device of the simulated host.
#[derive(Debug)]
pub struct SimDevice {
    pub kind: DeviceKind,
    pub name: DeviceName,
    /// What a read of it gives: a camera's frame, a display's pixels, a port's bytes.
    pub input: Vec<u8>,
    /// Everything written to it, in order.
    pub output: Vec<u8>,
}

/// What a device call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceOp {
    Read,
    Write,
    Scan,
    Fix,
    Post,
}

/// A device call that failed: the capability, the call, the device (where one is named) and
/// how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct DeviceError {
    /// The capability, as its type is called: `Camera`, `Usb`, ...
    pub capability: &'static str,
    pub op: DeviceOp,
    pub device: Option<DeviceName>,
    pub kind: Refused,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (capability, op) = (self.capability, self.op);
        match &self.device {
            Some(device) => write!(f, "bstd::device {capability} {op:?} of device {device}: {}", self.kind),
            None => write!(f, "bstd::device {capability} {op:?}: {}", self.kind),
        }
    }
}

impl core::error::Error for DeviceError {}

/// The simulated device `name` of `kind`.
fn device<'a>(host: &'a mut SimHost, kind: DeviceKind, name: &DeviceName) -> Result<&'a mut SimDevice, Refused> {
    host.devices.iter_mut().find(|it| it.kind == kind && &it.name == name).ok_or(Refused::NotFound)
}

/// `bytes` added to `into`, or the stop that it has no room.
fn extend(into: &mut Bytes<'_>, bytes: &[u8]) -> Result<usize, Refused> {
    into.extend(bytes)?;
    Ok(bytes.len())
}

/// What the device `name` of `kind` gives, added to `into`, through a capability that allows
/// `allowed`.
fn read(
    io: &Io,
    cx: &Cx,
    capability: &'static str,
    kind: DeviceKind,
    allowed: &Allowed<DeviceName>,
    name: &DeviceName,
    into: &mut Bytes<'_>,
) -> Result<usize, DeviceError> {
    // clone: an error's name of its device, a refcount.
    let refuse = |kind| DeviceError { capability, op: DeviceOp::Read, device: Some(name.clone()), kind };
    if !allowed.allows(name) {
        return Err(refuse(Refused::NotAllowed));
    }
    hosted::call(io, cx, |host| extend(into, &device(host, kind, name)?.input)).map_err(refuse)
}

/// `bytes` written to the device `name` of `kind`, through a capability that allows `allowed`.
fn write(
    io: &Io,
    cx: &Cx,
    capability: &'static str,
    kind: DeviceKind,
    allowed: &Allowed<DeviceName>,
    name: &DeviceName,
    bytes: &[u8],
) -> Result<(), DeviceError> {
    // clone: an error's name of its device, a refcount.
    let refuse = |kind| DeviceError { capability, op: DeviceOp::Write, device: Some(name.clone()), kind };
    if !allowed.allows(name) {
        return Err(refuse(Refused::NotAllowed));
    }
    hosted::call(io, cx, |host| {
        device(host, kind, name)?.output.extend_from_slice(bytes);
        Ok(())
    })
    .map_err(refuse)
}

/// Cameras: from [`Io::camera`]. Narrowed to a device.
#[derive(Clone)]
pub struct Camera {
    io: Io,
    allowed: Allowed<DeviceName>,
}

impl Camera {
    pub(crate) fn new(io: Io) -> Camera {
        Camera { io, allowed: Allowed::Any }
    }

    /// Only the camera `device`, if this reaches it already.
    pub fn only(&self, device: &DeviceName) -> Camera {
        Camera { io: self.io.clone(), allowed: self.allowed.one(device, self.io.plant()) }
    }

    /// One frame from the camera `device`, added to `into`; how many bytes.
    pub fn frame(&self, cx: &Cx, device: &DeviceName, into: &mut Bytes<'_>) -> Result<usize, DeviceError> {
        read(&self.io, cx, "Camera", DeviceKind::Camera, &self.allowed, device, into)
    }
}

/// What a [`Screen`] captures: a whole display, or one window.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount or a copy
pub enum Target {
    Display(DeviceName),
    Window(WindowId),
}

/// The screen's contents: from [`Io::screen`]. Narrowed to a display or a window.
#[derive(Clone)]
pub struct Screen {
    io: Io,
    allowed: Allowed<Target>,
}

impl Screen {
    pub(crate) fn new(io: Io) -> Screen {
        Screen { io, allowed: Allowed::Any }
    }

    /// Only `target`, if this captures it already.
    pub fn only(&self, target: &Target) -> Screen {
        Screen { io: self.io.clone(), allowed: self.allowed.one(target, self.io.plant()) }
    }

    /// What `target` shows now, added to `into`; how many bytes.
    pub fn capture(&self, cx: &Cx, target: &Target, into: &mut Bytes<'_>) -> Result<usize, DeviceError> {
        let named = match target {
            // clone: an error's name of its device, a refcount.
            Target::Display(display) => Some(display.clone()),
            Target::Window(_) => None,
        };
        let refuse = |kind| DeviceError { capability: "Screen", op: DeviceOp::Read, device: named, kind };
        if !self.allowed.allows(target) {
            return Err(refuse(Refused::NotAllowed));
        }
        let captured = hosted::call(&self.io, cx, |host| match target {
            Target::Display(display) => extend(into, &device(host, DeviceKind::Display, display)?.input),
            Target::Window(window) => match host.windows.iter().find(|it| it.id == *window) {
                Some(found) => extend(into, &found.pixels),
                None => Err(Refused::NotFound),
            },
        });
        captured.map_err(refuse)
    }
}

/// A place, in units of 1e-7 degrees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Position {
    pub lat_e7: i64,
    pub lon_e7: i64,
}

/// How finely a [`Location`] says where the machine is, finest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Precision {
    /// As the host knows it.
    Exact,
    /// Rounded to a hundredth of a degree, about a kilometre.
    City,
    /// Rounded to a degree, about a hundred kilometres.
    Country,
}

impl Precision {
    /// `position` as this precision lets it be known.
    pub fn round(self, position: Position) -> Position {
        let grain = match self {
            Precision::Exact => return position,
            Precision::City => 100_000,
            Precision::Country => 10_000_000,
        };
        let round = |at: i64| at.div_euclid(grain) * grain;
        Position { lat_e7: round(position.lat_e7), lon_e7: round(position.lon_e7) }
    }
}

/// Where the machine is: from [`Io::location`]. Narrowed to a precision.
#[derive(Clone)]
pub struct Location {
    io: Io,
    precision: Precision,
}

impl Location {
    pub(crate) fn new(io: Io) -> Location {
        Location { io, precision: Precision::Exact }
    }

    /// No finer than `precision`, and no finer than this is already.
    pub fn coarser(&self, precision: Precision) -> Location {
        let precision = match self.io.plant() {
            Plant::OnlyWidens => precision,
            _ => self.precision.max(precision),
        };
        Location { io: self.io.clone(), precision }
    }

    pub fn precision(&self) -> Precision {
        self.precision
    }

    /// Where the machine is, as finely as this may say.
    pub fn fix(&self, cx: &Cx) -> Result<Position, DeviceError> {
        hosted::call(&self.io, cx, |host| Ok(self.precision.round(host.position)))
            .map_err(|kind| DeviceError { capability: "Location", op: DeviceOp::Fix, device: None, kind })
    }
}

/// Bluetooth: from [`Io::bluetooth`]. Narrowed to a device, and to a service.
#[derive(Clone)]
pub struct Bluetooth {
    io: Io,
    devices: Allowed<DeviceName>,
    services: Allowed<ServiceName>,
}

impl Bluetooth {
    pub(crate) fn new(io: Io) -> Bluetooth {
        Bluetooth { io, devices: Allowed::Any, services: Allowed::Any }
    }

    /// Only the device `device`, if this reaches it already.
    pub fn only(&self, device: &DeviceName) -> Bluetooth {
        Bluetooth {
            io: self.io.clone(),
            devices: self.devices.one(device, self.io.plant()),
            // clone: authority held, kept: a refcount.
            services: self.services.clone(),
        }
    }

    /// Only the service `service`, if this reaches it already.
    pub fn only_service(&self, service: &ServiceName) -> Bluetooth {
        Bluetooth {
            io: self.io.clone(),
            // clone: authority held, kept: a refcount.
            devices: self.devices.clone(),
            services: self.services.one(service, self.io.plant()),
        }
    }

    /// The devices in range that this reaches, in `frame`.
    pub fn scan<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<DeviceName, &'a Frame>, DeviceError> {
        let mut found = Vec::new_in(frame);
        let scanned = hosted::call(&self.io, cx, |host| {
            let near = host.devices.iter().filter(|it| it.kind == DeviceKind::Bluetooth);
            for it in near.filter(|it| self.devices.allows(&it.name)) {
                found
                    .try_reserve(1)
                    .map_err(|_| Stopped::OutOfMemory { arena: "bstd::device scan", asked: size_of::<DeviceName>() })?;
                // clone: the device's name, a refcount.
                found.push(it.name.clone());
            }
            Ok(())
        });
        match scanned {
            Ok(()) => Ok(found),
            Err(kind) => Err(DeviceError { capability: "Bluetooth", op: DeviceOp::Scan, device: None, kind }),
        }
    }

    /// All of `bytes` written to the service `service` of the device `device`, or an error:
    /// never a part said written.
    pub fn write_all(&self, cx: &Cx, device: &DeviceName, service: &ServiceName, bytes: &[u8]) -> Result<(), DeviceError> {
        // clone: an error's name of its device, a refcount.
        let refuse = |kind| DeviceError { capability: "Bluetooth", op: DeviceOp::Write, device: Some(device.clone()), kind };
        if !self.devices.allows(device) || !self.services.allows(service) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            self::device(host, DeviceKind::Bluetooth, device)?;
            // clone: the simulated host's record: refcounts, and alloc: the bytes written.
            host.bluetooth_sent.push((device.clone(), service.clone(), bytes.to_vec()));
            Ok(())
        })
        .map_err(refuse)
    }
}

/// A capability over devices that are read from and written to as bytes, narrowed to one.
macro_rules! byte_device {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Clone)]
        pub struct $name {
            io: Io,
            allowed: Allowed<DeviceName>,
        }

        impl $name {
            pub(crate) fn new(io: Io) -> $name {
                $name { io, allowed: Allowed::Any }
            }

            /// Only the device `device`, if this reaches it already.
            pub fn only(&self, device: &DeviceName) -> $name {
                $name { io: self.io.clone(), allowed: self.allowed.one(device, self.io.plant()) }
            }

            /// What the device `device` sent, added to `into`; how many bytes.
            pub fn read(&self, cx: &Cx, device: &DeviceName, into: &mut Bytes<'_>) -> Result<usize, DeviceError> {
                read(&self.io, cx, stringify!($name), $kind, &self.allowed, device, into)
            }

            /// All of `bytes` written to the device `device`, or an error: never a part said
            /// written.
            pub fn write_all(&self, cx: &Cx, device: &DeviceName, bytes: &[u8]) -> Result<(), DeviceError> {
                write(&self.io, cx, stringify!($name), $kind, &self.allowed, device, bytes)
            }
        }
    };
}

byte_device! {
    /// USB devices: from [`Io::usb`]. Narrowed to a device.
    Usb, DeviceKind::Usb
}
byte_device! {
    /// Serial ports: from [`Io::serial`]. Narrowed to a device.
    Serial, DeviceKind::Serial
}

/// The clipboard: from [`Io::clipboard`]. Narrowed to reading or to writing.
#[derive(Clone)]
pub struct Clipboard {
    io: Io,
    reads: bool,
    writes: bool,
}

impl Clipboard {
    pub(crate) fn new(io: Io) -> Clipboard {
        Clipboard { io, reads: true, writes: true }
    }

    /// Reads only, if this reads already.
    pub fn read_only(&self) -> Clipboard {
        let reads = self.reads || self.io.plant() == Plant::OnlyWidens;
        Clipboard { io: self.io.clone(), reads, writes: false }
    }

    /// Writes only, if this writes already.
    pub fn write_only(&self) -> Clipboard {
        let writes = self.writes || self.io.plant() == Plant::OnlyWidens;
        Clipboard { io: self.io.clone(), reads: false, writes }
    }

    /// What the user last copied, added to `into`; how many bytes.
    pub fn read(&self, cx: &Cx, into: &mut Bytes<'_>) -> Result<usize, DeviceError> {
        let refuse = |kind| DeviceError { capability: "Clipboard", op: DeviceOp::Read, device: None, kind };
        if !self.reads {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| extend(into, &host.clipboard)).map_err(refuse)
    }

    /// `bytes` put on the clipboard, replacing what was there.
    pub fn write(&self, cx: &Cx, bytes: &[u8]) -> Result<(), DeviceError> {
        let refuse = |kind| DeviceError { capability: "Clipboard", op: DeviceOp::Write, device: None, kind };
        if !self.writes {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| {
            host.clipboard.clear();
            host.clipboard.extend_from_slice(bytes);
            Ok(())
        })
        .map_err(refuse)
    }
}

/// A notification: what [`Notify::post`] shows the user.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Notice<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

/// Notifications: from [`Io::notify`]. It has no narrowing.
#[derive(Clone)]
pub struct Notify {
    io: Io,
}

impl Notify {
    pub(crate) fn new(io: Io) -> Notify {
        Notify { io }
    }

    /// `notice` shown to the user.
    pub fn post(&self, cx: &Cx, notice: &Notice<'_>) -> Result<(), DeviceError> {
        hosted::call(&self.io, cx, |host| {
            // alloc: the simulated host's record of what was posted.
            host.notices.push((notice.title.to_owned(), notice.body.to_owned()));
            Ok(())
        })
        .map_err(|kind| DeviceError { capability: "Notify", op: DeviceOp::Post, device: None, kind })
    }
}
