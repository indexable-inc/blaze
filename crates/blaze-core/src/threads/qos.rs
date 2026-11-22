//! A thread's quality-of-service class on macOS (`pthread/qos.h`, public API since 10.10): the
//! one home of the classes blaze's own threads run at, and of the guard that holds the calling
//! thread at a class for a stretch of work and gives it back its own after.
//!
//! The classes in use, and why (thoughts.md 2.5):
//! - the pool's workers: `user-initiated`, or `default` where the process is lowered
//!   (blaze-graph pool.rs `worker_qos`, unchanged here);
//! - the work after the answer is printed ([`AFTER_ANSWER`]): `utility`, never above. Not
//!   `background`: a loaded host starves it (the finding kept at blaze-graph topology.rs
//!   `from_perflevels`: about 2% of a core at `utility` for the self-build's workers on
//!   2026-11-02, so `background`, below it, is worse); work after the answer is waited on by
//!   nobody, so `utility` is its floor and its ceiling.
//! - the kqueue driver's file threads, of either lane: the class of the thread that made the
//!   driver, and each effect at most its asker's (kqueue.rs `work_on`). The lane does not pick
//!   a class: a barrier a command's answer waits on is asked at the command's class and a
//!   `utility` thread's IO is throttled, so the class follows the asker, and an effect asked
//!   after the answer (inside a [`Lowered`] stretch) runs at `utility`.
//!
//! Linux has no classes. Its lowering (blaze-cas `lower_this_thread`: nice 19 and the idle IO
//! class) cannot be given back by an unprivileged thread (`RLIMIT_NICE` is 0 by default, so a
//! nice once raised stays), so a scoped [`Lowered`] exists on macOS only.

use core::marker::PhantomData;

use blaze_sys::posix::{proc as sys_proc, thread as sys_thread};

/// A QoS class (`qos_class_t`), each its value in `sys/qos.h`; a higher value is served first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Qos {
    UserInteractive = sys_proc::QOS_CLASS_USER_INTERACTIVE,
    UserInitiated = sys_proc::QOS_CLASS_USER_INITIATED,
    Default = sys_proc::QOS_CLASS_DEFAULT,
    Utility = sys_proc::QOS_CLASS_UTILITY,
    Background = sys_proc::QOS_CLASS_BACKGROUND,
    /// No class asked for: the thread is served as `default` or above (a process's main thread
    /// starts so).
    Unspecified = sys_proc::QOS_CLASS_UNSPECIFIED,
}

/// The class the work after the answer runs at: compaction, sealing, the segment digests
/// (blaze-cli store/plain_place.rs `after_answer`). The module doc says why `utility`.
pub const AFTER_ANSWER: Qos = Qos::Utility;

/// A QoS call the system refused: which call, and its error number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QosError {
    pub call: &'static str,
    pub errno: i32,
    /// The class the call was about (`Unspecified` for a read).
    pub class: Qos,
}

impl core::fmt::Display for QosError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} of the calling thread ({:?}) failed with error {}", self.call, self.class, self.errno)
    }
}

impl core::error::Error for QosError {}

impl From<QosError> for std::io::Error {
    fn from(error: QosError) -> std::io::Error {
        let why = std::io::Error::from_raw_os_error(error.errno);
        // alloc: an error path.
        std::io::Error::new(why.kind(), error.to_string())
    }
}

impl Qos {
    /// Its `qos_class_t` value.
    pub const fn raw(self) -> u32 {
        self as u32
    }

    /// The class of a `qos_class_t` the system reported; `None` for a value `sys/qos.h` does
    /// not list.
    pub const fn of_raw(raw: u32) -> Option<Qos> {
        match raw {
            sys_proc::QOS_CLASS_USER_INTERACTIVE => Some(Qos::UserInteractive),
            sys_proc::QOS_CLASS_USER_INITIATED => Some(Qos::UserInitiated),
            sys_proc::QOS_CLASS_DEFAULT => Some(Qos::Default),
            sys_proc::QOS_CLASS_UTILITY => Some(Qos::Utility),
            sys_proc::QOS_CLASS_BACKGROUND => Some(Qos::Background),
            sys_proc::QOS_CLASS_UNSPECIFIED => Some(Qos::Unspecified),
            _ => None,
        }
    }

    /// The class a thread that was at `self` is set to or given back: `self`, but `default` for
    /// `Unspecified`, which `pthread_set_qos_class_self_np` does not take (its doc in the SDK's
    /// `pthread/qos.h` lists the five classes asked for: "EINVAL will be returned if any other
    /// value is provided"; read 2026-11-21) and which the
    /// system serves at `default` in a process with no class of its own, as blaze's are.
    pub const fn settable(self) -> Qos {
        match self {
            Qos::Unspecified => Qos::Default,
            other => other,
        }
    }

    /// Whether a thread at `self` is served before one at `other`. `Unspecified` counts as
    /// above every class asked for: the system serves it at `default` or the process's own,
    /// so a thread at it is lowered by any class below `default`.
    pub const fn above(self, other: Qos) -> bool {
        const fn rank(class: Qos) -> u32 {
            match class {
                Qos::Unspecified => u32::MAX,
                other => other.raw(),
            }
        }
        rank(self) > rank(other)
    }
}

/// The calling thread's class and relative priority (`pthread_get_qos_class_np`).
pub fn this_thread() -> Result<(Qos, i32), QosError> {
    let mut class = sys_proc::QOS_CLASS_UNSPECIFIED;
    let mut relative = 0;
    // SAFETY: the calling thread's own handle; the two locals are written by the call.
    let got = unsafe { sys_thread::pthread_get_qos_class_np(sys_thread::pthread_self(), &mut class, &mut relative) };
    if got != 0 {
        return Err(QosError { call: "pthread_get_qos_class_np", errno: got, class: Qos::Unspecified });
    }
    // The kernel holds only the classes `sys/qos.h` lists.
    let class = Qos::of_raw(class).ok_or(QosError { call: "pthread_get_qos_class_np (a class sys/qos.h does not list)", errno: 0, class: Qos::Unspecified })?;
    Ok((class, relative))
}

/// The calling thread set to `class` at relative priority `relative` (0 or below)
/// (`pthread_set_qos_class_self_np`). A thread's class is inherited by the threads it starts.
pub fn set_this_thread(class: Qos, relative: i32) -> Result<(), QosError> {
    // SAFETY: sets the calling thread's own class; no memory is passed.
    let got = unsafe { sys_thread::pthread_set_qos_class_self_np(class.raw(), relative) };
    if got != 0 {
        return Err(QosError { call: "pthread_set_qos_class_self_np", errno: got, class });
    }
    Ok(())
}

/// The calling thread held at most at a class until this is dropped, then given back the class
/// and relative priority it had, on every way out of the stretch: a return, an early `?`, and a
/// panic's unwinding. Not `Send`: it is about the thread that made it.
#[must_use = "the class is given back when this is dropped"]
pub struct Lowered {
    /// What the thread had, given back at the drop; `None` where it was at or below the class
    /// already and nothing was changed.
    was: Option<(Qos, i32)>,
    _thread: PhantomData<*const ()>,
}

impl Lowered {
    /// The calling thread at `class` where it is above it; left as it is where it is at or
    /// below it (never raised: "at most").
    pub fn at_most(class: Qos) -> Result<Lowered, QosError> {
        let (now, relative) = this_thread()?;
        if !now.above(class) {
            return Ok(Lowered { was: None, _thread: PhantomData });
        }
        set_this_thread(class, 0)?;
        // An unspecified thread is given back `default` ([`Qos::settable`]).
        Ok(Lowered { was: Some((now.settable(), relative)), _thread: PhantomData })
    }

    /// The class it gives back at its drop, if it changed the thread's.
    pub fn was(&self) -> Option<Qos> {
        self.was.map(|(class, _)| class)
    }
}

impl Drop for Lowered {
    fn drop(&mut self) {
        let Some((class, relative)) = self.was else { return };
        // Giving a thread back a class it had is refused only for a class the system no
        // longer allows the process (none blaze asks for): a typed failure of this job, unless
        // the thread is unwinding a panic already, which fails the job itself; a second panic
        // there would abort the process, and the class left lowered slows only this thread.
        if let Err(error) = set_this_thread(class, relative)
            && !std::thread::panicking()
        {
            panic!("the class the thread had before a stretch lowered to it was not given back: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The after-answer class is `utility` (0x11); an unspecified thread is given back `default`.
    #[test]
    fn the_after_answer_class_is_utility() {
        assert_eq!(AFTER_ANSWER, Qos::Utility);
        assert_eq!(AFTER_ANSWER.raw(), 0x11);
        assert_eq!(Qos::Unspecified.settable(), Qos::Default);
        assert_eq!(Qos::Utility.settable(), Qos::Utility);
        assert!(Qos::Default.above(AFTER_ANSWER) && AFTER_ANSWER.above(Qos::Background));
        for class in [Qos::UserInteractive, Qos::UserInitiated, Qos::Default, Qos::Utility, Qos::Background, Qos::Unspecified] {
            assert_eq!(Qos::of_raw(class.raw()), Some(class));
        }
    }

    /// A stretch lowered gives the thread back its class on a return and on a panic; a thread
    /// below the class is not raised. Control: the class does change inside the stretch.
    #[test]
    fn a_lowered_stretch_gives_back_the_class_on_every_way_out() {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    set_this_thread(Qos::UserInitiated, 0).unwrap();
                    {
                        let lowered = Lowered::at_most(Qos::Utility).unwrap();
                        assert_eq!(lowered.was(), Some(Qos::UserInitiated));
                        assert_eq!(this_thread().unwrap().0, Qos::Utility, "lowered inside the stretch");
                    }
                    assert_eq!(this_thread().unwrap().0, Qos::UserInitiated, "given back on a return");
                    let unwound = std::panic::catch_unwind(|| {
                        let _lowered = Lowered::at_most(Qos::Utility).unwrap();
                        assert_eq!(this_thread().unwrap().0, Qos::Utility);
                        panic!("a job that fails inside the stretch");
                    });
                    assert!(unwound.is_err());
                    assert_eq!(this_thread().unwrap().0, Qos::UserInitiated, "given back on a panic");
                    set_this_thread(Qos::Background, 0).unwrap();
                    {
                        let lowered = Lowered::at_most(Qos::Utility).unwrap();
                        assert_eq!(lowered.was(), None);
                        assert_eq!(this_thread().unwrap().0, Qos::Background, "never raised");
                    }
                    assert_eq!(this_thread().unwrap().0, Qos::Background);
                })
                .join()
                .unwrap();
        });
    }
}
