//! Sound (docs/v2/plan/design/std.md, "Sound, and the device capabilities"): [`Audio`], from
//! [`Io::audio`], gives [`AudioOut`] and, separately, [`AudioIn`]: a microphone is as sensitive
//! as the file system, so listening is its own capability, which a host may withhold while it
//! offers playing ([`Audio::input`]). Each is narrowed to a device by its `only`, which never
//! widens.
//!
//! **The stream is pulled by the device.** [`AudioOut::play`] and [`AudioIn::record`] run a
//! stream of [`Pcm`] samples for a number of periods: each period the device calls the
//! callback with a buffer of [`PERIOD`] samples to fill ([`Fill`]) or to read ([`Take`]).
//! **The callback must be `nonblocking` and `noalloc`**: it runs where the audio thread may
//! never wait, so it takes no `Cx` and no capability, and talks to the rest of the program
//! through a bounded queue it was built over. (The checks of those two effects on the
//! callback's body are crates/hir-ty-pure's, not yet wired to this type.)
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::audio`]): what is played is
//! captured into a buffer the test reads, and what is recorded is scripted, silence once the
//! script runs out. blaze-core has no effect for a sound device (CoreAudio, PipeWire, WASAPI).
//! Sample times are data: a simulated run gives exact samples a test compares.

use alloc::{collections::VecDeque, vec::Vec};
use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::{Io, Missing},
    name::DeviceName,
    narrow::Allowed,
};

/// A stream's format: signed 16-bit samples, interleaved by channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pcm {
    /// Frames a second.
    pub rate: u32,
    pub channels: u16,
}

impl Pcm {
    /// Why the device takes no such format, if it does not.
    fn refused(self) -> Option<Refused> {
        match (self.rate, self.channels) {
            (0, _) => Some(Refused::Host("a stream of 0 frames a second")),
            (_, 0) => Some(Refused::Host("a stream of no channels")),
            (_, 9..) => Some(Refused::Host("a stream of more than 8 channels")),
            _ => None,
        }
    }
}

/// The samples one pull hands its callback.
pub const PERIOD: usize = 256;

/// What an output stream's device calls to fill each period. Required `nonblocking` and
/// `noalloc` (see the module's doc): it may not wait, allocate or touch a capability.
pub type Fill<'a> = dyn FnMut(&mut [i16]) + 'a;

/// What an input stream's device calls with each period it recorded. Required `nonblocking`
/// and `noalloc`, as [`Fill`].
pub type Take<'a> = dyn FnMut(&[i16]) + 'a;

/// The simulated sound devices.
#[derive(Debug)]
pub struct SimAudio {
    pub outputs: Vec<DeviceName>,
    pub inputs: Vec<DeviceName>,
    /// Every sample played, in order.
    pub played: Vec<i16>,
    /// The samples a recording will hear, in order; silence after.
    pub heard: VecDeque<i16>,
}

impl SimAudio {
    /// No device, nothing played, nothing to hear.
    #[blaze::nonblocking]
    pub fn new() -> SimAudio {
        // alloc: none, the simulated devices' empty tables.
        SimAudio { outputs: Vec::new(), inputs: Vec::new(), played: Vec::new(), heard: VecDeque::new() }
    }
}

impl Default for SimAudio {
    #[blaze::nonblocking]
fn default() -> SimAudio {
        SimAudio::new()
    }
}

/// What a sound call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioOp {
    Play,
    Record,
}

/// A sound call that failed: the call, the device and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct AudioError {
    pub op: AudioOp,
    pub device: DeviceName,
    pub kind: Refused,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::audio {:?} on device {}: {}", self.op, self.device, self.kind)
    }
}

impl core::error::Error for AudioError {}

/// The host's sound: from [`Io::audio`].
#[derive(Clone)]
pub struct Audio {
    io: Io,
}

impl Audio {
    #[blaze::nopanic]
pub(crate) fn new(io: Io) -> Audio {
        Audio { io }
    }

    /// Playing sound, on any output device.
    pub fn out(&self) -> AudioOut {
        AudioOut { io: self.io.clone(), allowed: Allowed::Any }
    }

    /// Recording sound, if the host offers it: its own entry of [`crate::io::Offers`]
    /// (`audio_in`), which a host that plays sound may still withhold.
    pub fn input(&self) -> Option<AudioIn> {
        self.io.offered(|it| it.audio_in).then(|| AudioIn { io: self.io.clone(), allowed: Allowed::Any })
    }

    /// Recording sound, which `main` requires.
    pub fn need_input(&self) -> Result<AudioIn, Missing> {
        self.input().ok_or(Missing { capability: "audio_in" })
    }
}

impl fmt::Debug for Audio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Audio")
    }
}

/// Playing sound: from [`Audio::out`].
#[derive(Clone)]
pub struct AudioOut {
    io: Io,
    allowed: Allowed<DeviceName>,
}

impl AudioOut {
    /// Only the device `device`, if this plays on it already.
    pub fn only(&self, device: &DeviceName) -> AudioOut {
        AudioOut { io: self.io.clone(), allowed: self.allowed.one(device, self.io.plant()) }
    }

    /// A stream of `pcm` played on `device` for `periods` periods: each period the device
    /// calls `fill` with [`PERIOD`] samples to fill, then plays them. How many samples it
    /// played. It ends early, with what stopped it, once `cx` has.
    pub fn play(
        &self,
        cx: &Cx,
        device: &DeviceName,
        pcm: Pcm,
        periods: usize,
        fill: &mut Fill<'_>,
    ) -> Result<usize, AudioError> {
        // clone: an error's name of its device, a refcount.
        let refuse = |kind| AudioError { op: AudioOp::Play, device: device.clone(), kind };
        if !self.allowed.allows(device) {
            return Err(refuse(Refused::NotAllowed));
        }
        if let Some(why) = pcm.refused() {
            return Err(refuse(why));
        }
        let cx = cx.span("bstd::audio play");
        let mut played = 0;
        for _ in 0..periods {
            let mut period = [0i16; PERIOD];
            // The device's pull: nothing is held while the callback runs.
            fill(&mut period);
            hosted::call(&self.io, &cx, |host| {
                if !host.audio.outputs.contains(device) {
                    return Err(Refused::NotFound);
                }
                host.audio.played.extend_from_slice(&period);
                Ok(())
            })
            .map_err(&refuse)?;
            played += PERIOD;
        }
        Ok(played)
    }
}

impl fmt::Debug for AudioOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AudioOut({:?})", self.allowed)
    }
}

/// Recording sound: from [`Audio::input`].
#[derive(Clone)]
pub struct AudioIn {
    io: Io,
    allowed: Allowed<DeviceName>,
}

impl AudioIn {
    /// Only the device `device`, if this records from it already.
    pub fn only(&self, device: &DeviceName) -> AudioIn {
        AudioIn { io: self.io.clone(), allowed: self.allowed.one(device, self.io.plant()) }
    }

    /// A stream of `pcm` recorded from `device` for `periods` periods: each period the device
    /// records [`PERIOD`] samples, then calls `take` with them. How many samples it recorded.
    /// It ends early, with what stopped it, once `cx` has.
    pub fn record(
        &self,
        cx: &Cx,
        device: &DeviceName,
        pcm: Pcm,
        periods: usize,
        take: &mut Take<'_>,
    ) -> Result<usize, AudioError> {
        // clone: an error's name of its device, a refcount.
        let refuse = |kind| AudioError { op: AudioOp::Record, device: device.clone(), kind };
        if !self.allowed.allows(device) {
            return Err(refuse(Refused::NotAllowed));
        }
        if let Some(why) = pcm.refused() {
            return Err(refuse(why));
        }
        let cx = cx.span("bstd::audio record");
        let mut recorded = 0;
        for _ in 0..periods {
            let mut period = [0i16; PERIOD];
            hosted::call(&self.io, &cx, |host| {
                if !host.audio.inputs.contains(device) {
                    return Err(Refused::NotFound);
                }
                for sample in &mut period {
                    // Silence once the script runs out.
                    *sample = host.audio.heard.pop_front().unwrap_or(0);
                }
                Ok(())
            })
            .map_err(&refuse)?;
            // The device's call: nothing is held while the callback runs.
            take(&period);
            recorded += PERIOD;
        }
        Ok(recorded)
    }
}

impl fmt::Debug for AudioIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AudioIn({:?})", self.allowed)
    }
}
