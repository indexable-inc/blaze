//! What still comes from the machine (D192): the short named list of the spec, as a closed
//! enum. Everything else a step runs or reads is the pinned set's or the step's own inputs. A
//! third thing taken from the machine is a new variant here, so every `match` on the list (the
//! key, the report, the sandbox grant) is a visible edit, never a quiet widening.
//!
//! Each entry's identity is in the key of every step that used it ([`FromMachine::identity_into`],
//! taken by [`crate::StepTools::finish`]): the Apple SDK (headers and `.tbd` stubs, which Apple
//! does not let anyone redistribute) by its `Version` and `ProductBuildVersion`, and the system
//! libraries a process must load (libSystem, dyld; the kernel) by the OS build. The reader of
//! each is a driver elsewhere (blaze-build-script `fact_pins` reads `SystemVersion.plist`; the
//! SDK's `SDKSettings.json` and `SystemVersion.plist`); this module holds only the values.

use core::fmt;

/// The Apple SDK a step read, by its identity: `SDKSettings.json`'s `Version` (`27.2`) and its
/// `System/Library/CoreServices/SystemVersion.plist`'s `ProductBuildVersion` (`26B118`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SdkId<'m> {
    version: &'m str,
    build: &'m str,
}

/// The OS build whose system libraries a process loads: `SystemVersion.plist`'s
/// `ProductBuildVersion` (`26A434`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OsBuild<'m> {
    build: &'m str,
}

/// An identity not of its form: what it was meant to be and the text found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachineIdError<'m> {
    pub what: &'static str,
    pub text: &'m str,
}

impl fmt::Display for MachineIdError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not of the form of {} (D192: what comes from the machine is keyed by its identity)", self.text, self.what)
    }
}

impl core::error::Error for MachineIdError<'_> {}

/// Dotted decimal numbers: `27.2`, `20.0.0`.
fn dotted(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// A build: ASCII letters and digits, as Apple's `ProductBuildVersion` is (`26B5044k`).
fn build_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= 32 && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl<'m> SdkId<'m> {
    pub fn new(version: &'m str, build: &'m str) -> Result<SdkId<'m>, MachineIdError<'m>> {
        if !dotted(version) {
            return Err(MachineIdError { what: "an SDK's Version", text: version });
        }
        if !build_ok(build) {
            return Err(MachineIdError { what: "an SDK's ProductBuildVersion", text: build });
        }
        Ok(SdkId { version, build })
    }

    pub fn version(&self) -> &'m str {
        self.version
    }

    pub fn build(&self) -> &'m str {
        self.build
    }
}

impl<'m> OsBuild<'m> {
    pub fn new(build: &'m str) -> Result<OsBuild<'m>, MachineIdError<'m>> {
        if !build_ok(build) {
            return Err(MachineIdError { what: "an OS build (ProductBuildVersion)", text: build });
        }
        Ok(OsBuild { build })
    }

    pub fn build(&self) -> &'m str {
        self.build
    }
}

/// What a step took from the machine. Closed (see the module's doc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FromMachine<'m> {
    /// The Apple SDK's headers and stubs, read by a compile or by libclang.
    AppleSdk(SdkId<'m>),
    /// The system libraries every process loads (libSystem, dyld) and the kernel under them:
    /// held by every step that ran a program on an Apple platform, whose programs cannot be
    /// linked without libSystem.
    SystemLibraries(OsBuild<'m>),
}

impl FromMachine<'_> {
    /// The entry's tag in the key: one byte per variant, never reused.
    pub const fn tag(&self) -> u8 {
        match self {
            FromMachine::AppleSdk(_) => 1,
            FromMachine::SystemLibraries(_) => 2,
        }
    }

    /// The entry's name, as a report prints it.
    pub const fn what(&self) -> &'static str {
        match self {
            FromMachine::AppleSdk(_) => "the Apple SDK",
            FromMachine::SystemLibraries(_) => "the system libraries",
        }
    }

    /// The entry's identity bytes for a key, handed to `out` in order: the tag, then each field
    /// as its length (8 bytes, little-endian) and its bytes, so no two entries give one stream.
    pub fn identity_into(&self, out: &mut impl FnMut(&[u8])) {
        out(&[self.tag()]);
        let mut field = |text: &str| {
            out(&(text.len() as u64).to_le_bytes());
            out(text.as_bytes());
        };
        match self {
            FromMachine::AppleSdk(sdk) => {
                field(sdk.version);
                field(sdk.build);
            }
            FromMachine::SystemLibraries(os) => field(os.build),
        }
    }
}

impl fmt::Display for FromMachine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FromMachine::AppleSdk(sdk) => write!(f, "the Apple SDK {} ({})", sdk.version, sdk.build),
            FromMachine::SystemLibraries(os) => write!(f, "the system libraries of OS build {}", os.build),
        }
    }
}

/// The machine a step runs on, as far as the set's programs depend on it: on Apple, the OS
/// build whose libSystem they load; on Linux nothing (the set's programs are linked statically
/// and ask only the kernel's stable system calls).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MachineOs<'m> {
    Apple(OsBuild<'m>),
    Linux,
}
