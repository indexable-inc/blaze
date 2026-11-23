//! Device grants narrowed to one device: `main` takes [`AudioOut`], [`AudioIn`], [`Camera`],
//! [`Bluetooth`], [`Usb`] or [`Serial`], and its grant is either the capability whole
//! (`--grant mic=whole`) or one device of it (`--grant mic=device:<name>`, the spec's
//! `{"whole": {"capability": "audio_in", "device": "<name>"}}`). A device grant is the
//! capability's own `only(device)` applied before `main` runs, so it never widens and `main`
//! cannot reach another device: a call naming one is refused with `NotAllowed`.
//!
//! Each kind is its root capability's name in [`crate::io::Offers`]: playing is `audio` (the
//! flag that means playing; [`crate::audio::Audio`], which can also ask for input, is `audio`
//! too, and takes only the whole grant), recording is `audio_in`, a host's own entry because a
//! microphone is as sensitive as the file system.
//!
//! **Which device capabilities have a narrower half.**
//!
//! | Type          | Grant kind   | Narrower by the grant | The type's own narrowing          |
//! |---------------|--------------|-----------------------|-----------------------------------|
//! | `AudioOut`    | `audio`      | one device            | `only(&DeviceName)`               |
//! | `AudioIn`     | `audio_in`   | one device            | `only(&DeviceName)`               |
//! | `Camera`      | `camera`     | one device            | `only(&DeviceName)`               |
//! | `Bluetooth`   | `bluetooth`  | one device            | `only`, and `only_service`        |
//! | `Usb`         | `usb`        | one device            | `only(&DeviceName)`               |
//! | `Serial`      | `serial`     | one device            | `only(&DeviceName)`               |
//! | `Screen`      | `screen`     | whole only            | `only(&Target)`, a display/window |
//! | `Location`    | `location`   | whole only            | `coarser(Precision)`              |
//! | `Clipboard`   | `clipboard`  | whole only            | `read_only()`, `write_only()`     |
//! | `Notify`      | `notify`     | whole only            | none: it only posts               |
//! | `Gpu`         | `gpu`        | whole only            | none yet (gpu.rs, a placeholder)  |
//!
//! The four "whole only" rows narrow by something that is not a device name; each wants its own
//! scope spelling (`clipboard:read`, `location:city`, a display) before a grant can say it.
//! Until then `main` narrows them itself, after the start, with the type's own method.
//!
//! **Simulation only.** A real machine offers none of these yet ([`crate::io::Offers::NATIVE`]
//! has no sound or device flag: blaze-core has no effect for them; the `Gpu` is the native
//! macOS world's only), so on one a program that asks for one is refused before `main`, by the
//! capability's name: "this host offers no `audio_in`". The simulation offers each.

use super::{Grant, Granting, Kind, Want};
use crate::{
    audio::{AudioIn, AudioOut},
    device::{Bluetooth, Camera, Serial, Usb},
    name::DeviceName,
    spec::Scope,
};

/// The root capabilities a grant may narrow to one device, by their names in
/// [`crate::io::Offers::NAMES`]. The spec's reader and `--grant`'s refuse a device for any other.
pub const ONE_DEVICE: [&str; 6] = ["audio", "audio_in", "camera", "bluetooth", "usb", "serial"];

/// The whole capabilities a world has or lacks by its machine, not by a host's offer, so they
/// are not [`crate::io::Offers`] flags (whose accessor is `Some` exactly when the flag is set):
/// a `Gpu` is had on a native macOS world and nowhere else yet ([`crate::io::Io::gpu`]); a
/// `Hermetic` ([`crate::procs::hermetic::Hermetic`], D177) wherever the host's spawner is, made
/// from it and the clock, and narrower than either: it starts only confined children.
pub const BY_MACHINE: [&str; 2] = ["gpu", "hermetic"];

/// Whether `name` is a capability a whole grant may name: an [`crate::io::Offers::NAMES`] name
/// or one of [`BY_MACHINE`]. The spec's reader asks it.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn is_whole_capability(name: &str) -> bool {
    crate::io::Offers::NAMES.contains(&name) || BY_MACHINE.contains(&name)
}

/// Whether `capability` (an [`crate::io::Offers::NAMES`] name) is narrowed to one device by a
/// grant: one of [`ONE_DEVICE`].
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn narrows_to_device(capability: &str) -> bool {
    let mut at = 0;
    while at < ONE_DEVICE.len() {
        if super::same(ONE_DEVICE[at], capability) {
            return true;
        }
        at += 1;
    }
    false
}

/// A capability a grant narrows to one device: how it is had whole from its world, and its own
/// narrowing.
trait OneDevice: Sized {
    /// Its root capability's name, in [`ONE_DEVICE`].
    const CAPABILITY: &'static str;

    /// The capability whole, from the grant's world.
    fn whole(io: &crate::io::Io) -> Result<Self, crate::io::Missing>;

    /// The capability narrowed to `device`.
    fn only(&self, device: &DeviceName) -> Self;
}

/// The one making of every [`OneDevice`] grant: the whole grant gives it whole, the device grant
/// gives its `only(device)`; a grant of another capability is the wrong kind.
fn grant_one<T: OneDevice>(from: &Granting<'_>) -> Result<T, Want> {
    // An ordinary start with no grant of it: the process's capability whole, as `whole!` gives
    // (coordinator's decision 1). A grant it does carry (`--grant mic=device:<name>`, or the
    // attribute's default) is read below, so a device grant narrows there too.
    if from.started() == super::Started::Ordinary && from.granted().is_none() {
        return T::whole(from.process()).map_err(Want::NotOffered);
    }
    let (scope, io) = from.scope("whole")?;
    let Scope::Whole { capability, device } = scope else { return Err(Want::WrongKind { given: scope.kind_name() }) };
    if capability.as_str() != T::CAPABILITY {
        return Err(Want::WrongKind { given: scope.kind_name() });
    }
    let whole = T::whole(io).map_err(Want::NotOffered)?;
    let Some(device) = device else { return Ok(whole) };
    // The spec's reader checked the name; a name it let through and this refuses is said, not
    // dropped.
    let device = DeviceName::new(device.as_str()).map_err(|why| {
        let mut said = blaze_own::String::new_in(from.alloc());
        blaze_text::PutDisplay::put_display(&mut said, &why);
        Want::Failed(said)
    })?;
    Ok(whole.only(&device))
}

macro_rules! one_device {
    ($($ty:ty => $name:literal, |$io:ident| $whole:expr;)*) => {$(
        impl OneDevice for $ty {
            const CAPABILITY: &'static str = $name;

            fn whole($io: &crate::io::Io) -> Result<$ty, crate::io::Missing> {
                $whole
            }

            fn only(&self, device: &DeviceName) -> $ty {
                <$ty>::only(self, device)
            }
        }

        impl Grant for $ty {
            const KIND: Kind = Kind::Whole($name);

            fn grant(from: &Granting<'_>) -> Result<$ty, Want> {
                grant_one::<$ty>(from)
            }
        }
    )*};
}

one_device! {
    AudioOut => "audio", |io| io.need_audio().map(|audio| audio.out());
    AudioIn => "audio_in", |io| io.need_audio().and_then(|audio| audio.need_input());
    Camera => "camera", |io| io.need_camera();
    Bluetooth => "bluetooth", |io| io.need_bluetooth();
    Usb => "usb", |io| io.need_usb();
    Serial => "serial", |io| io.need_serial();
}

#[cfg(test)]
mod tests;
