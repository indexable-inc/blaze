//! The `libc` crate as the oracle for this module: every `#[repr(C)]` type's size, alignment
//! and read offsets, every constant's value, the signatures that are the same types, and a few
//! calls made both ways (on Linux: our system calls against glibc's functions). Deleted with
//! the `libc` dev-dependency (D180).

#[cfg(target_os = "macos")]
use core::ffi::{c_char, c_int, c_long, c_void};
use core::mem::offset_of;

use super::*;

/// Size and alignment of ours against libc's.
macro_rules! same_shape {
    ($ours:ty, $theirs:ty) => {
        assert_eq!(size_of::<$ours>(), size_of::<$theirs>(), concat!("size of ", stringify!($ours)));
        assert_eq!(align_of::<$ours>(), align_of::<$theirs>(), concat!("align of ", stringify!($ours)));
    };
}

/// Each named field at libc's offset.
macro_rules! same_offsets {
    ($ours:ty, $theirs:ty, $($field:ident),*) => {
        $(assert_eq!(
            offset_of!($ours, $field),
            offset_of!($theirs, $field),
            concat!(stringify!($ours), ".", stringify!($field)),
        );)*
    };
}

#[test]
fn the_layouts_are_libcs() {
    same_shape!(utsname, libc::utsname);
    same_offsets!(utsname, libc::utsname, sysname, nodename, release, version, machine);
    same_shape!(rlim_t, libc::rlim_t);
    same_shape!(rlimit, libc::rlimit);
    same_offsets!(rlimit, libc::rlimit, rlim_cur, rlim_max);
    same_shape!(rusage, libc::rusage);
}

#[cfg(target_os = "linux")]
#[test]
fn the_linux_layouts_are_libcs() {
    same_offsets!(utsname, libc::utsname, domainname);
    same_shape!(cpu_set_t, libc::cpu_set_t);
}

#[cfg(target_os = "macos")]
#[test]
fn the_macos_layouts_are_libcs() {
    // Size only for `pthread_t`, where libc's is an integer: both are one word.
    same_shape!(pthread_t, libc::pthread_t);
    same_shape!(pthread_key_t, libc::pthread_key_t);
    same_shape!(pthread_attr_t, libc::pthread_attr_t);
    same_shape!(pthread_mutex_t, libc::pthread_mutex_t);
    same_shape!(pthread_cond_t, libc::pthread_cond_t);
    same_shape!(Dl_info, libc::Dl_info);
    same_offsets!(Dl_info, libc::Dl_info, dli_fname, dli_fbase, dli_sname, dli_saddr);
    same_shape!(passwd, libc::passwd);
    same_offsets!(passwd, libc::passwd, pw_name, pw_passwd, pw_uid, pw_gid, pw_gecos, pw_dir, pw_shell);
    same_offsets!(passwd, libc::passwd, pw_change, pw_class, pw_expire);
    same_shape!(mach_port_t, libc::mach_port_t);
    same_shape!(thread_flavor_t, libc::thread_flavor_t);
    same_shape!(mach_msg_type_number_t, libc::mach_msg_type_number_t);
    same_shape!(thread_basic_info, libc::thread_basic_info);
    same_offsets!(
        thread_basic_info,
        libc::thread_basic_info,
        user_time,
        system_time,
        cpu_usage,
        policy,
        run_state,
        flags,
        suspend_count,
        sleep_time
    );
    same_shape!(mach_task_basic_info, libc::mach_task_basic_info);
    same_offsets!(
        mach_task_basic_info,
        libc::mach_task_basic_info,
        virtual_size,
        resident_size,
        resident_size_max,
        user_time,
        system_time,
        policy,
        suspend_count
    );
    same_shape!(proc_taskinfo, libc::proc_taskinfo);
    same_offsets!(
        proc_taskinfo,
        libc::proc_taskinfo,
        pti_virtual_size,
        pti_resident_size,
        pti_total_user,
        pti_total_system,
        pti_threads_user,
        pti_threads_system,
        pti_policy,
        pti_faults,
        pti_threadnum,
        pti_numrunning,
        pti_priority
    );
    same_shape!(proc_fdinfo, libc::proc_fdinfo);
    same_offsets!(proc_fdinfo, libc::proc_fdinfo, proc_fd, proc_fdtype);
    same_shape!(rusage_info_v4, libc::rusage_info_v4);
}

#[cfg(target_os = "macos")]
#[test]
fn the_initializers_are_libcs() {
    /// A value's bytes.
    fn bytes<T>(value: &T) -> &[u8] {
        // SAFETY: `value` is live for the borrow and `size_of::<T>()` bytes long; the types here
        // are plain C data with no padding read as uninitialized (signatures and byte arrays).
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
    }
    assert_eq!(bytes(&PTHREAD_MUTEX_INITIALIZER), bytes(&libc::PTHREAD_MUTEX_INITIALIZER));
    assert_eq!(bytes(&PTHREAD_COND_INITIALIZER), bytes(&libc::PTHREAD_COND_INITIALIZER));
}

#[test]
fn the_constants_are_libcs() {
    // glibc's `RLIMIT_*` is an `unsigned int` enum; compared by value.
    assert_eq!(i64::from(RLIMIT_NOFILE), libc::RLIMIT_NOFILE as i64);
    assert_eq!(RLIM_INFINITY, libc::RLIM_INFINITY);
    assert_eq!(RUSAGE_SELF, libc::RUSAGE_SELF);
}

#[cfg(target_os = "linux")]
#[test]
fn the_linux_constants_are_libcs() {
    use crate::linux::host::Resource;
    assert_eq!(CPU_SETSIZE, libc::CPU_SETSIZE as usize);
    // The `*at` numbers blaze-sys's Linux wrappers already carry.
    assert_eq!(crate::linux::CWD.raw(), libc::AT_FDCWD);
    let resources: &[(Resource, u32, &str)] = &[
        (Resource::CPU, libc::RLIMIT_CPU as u32, "RLIMIT_CPU"),
        (Resource::FSIZE, libc::RLIMIT_FSIZE as u32, "RLIMIT_FSIZE"),
        (Resource::DATA, libc::RLIMIT_DATA as u32, "RLIMIT_DATA"),
        (Resource::STACK, libc::RLIMIT_STACK as u32, "RLIMIT_STACK"),
        (Resource::CORE, libc::RLIMIT_CORE as u32, "RLIMIT_CORE"),
        (Resource::NPROC, libc::RLIMIT_NPROC as u32, "RLIMIT_NPROC"),
        (Resource::NOFILE, libc::RLIMIT_NOFILE as u32, "RLIMIT_NOFILE"),
        (Resource::MEMLOCK, libc::RLIMIT_MEMLOCK as u32, "RLIMIT_MEMLOCK"),
        (Resource::AS, libc::RLIMIT_AS as u32, "RLIMIT_AS"),
    ];
    for &(ours, libcs, name) in resources {
        assert_eq!(ours.0, libcs, "{name}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_macos_constants_are_libcs() {
    assert_eq!(RTLD_DEFAULT, libc::RTLD_DEFAULT);
    assert_eq!(RtldFlags::LAZY.bits(), libc::RTLD_LAZY);
    assert_eq!(RtldFlags::NOW.bits(), libc::RTLD_NOW);
    assert_eq!(RtldFlags::LOCAL.bits(), libc::RTLD_LOCAL);
    assert_eq!(RtldFlags::GLOBAL.bits(), libc::RTLD_GLOBAL);
    assert_eq!(RtldFlags::NOLOAD.bits(), libc::RTLD_NOLOAD);
    assert_eq!(_SC_PAGESIZE, libc::_SC_PAGESIZE);
    assert_eq!(_SC_CLK_TCK, libc::_SC_CLK_TCK);
    assert_eq!(_SC_GETPW_R_SIZE_MAX, libc::_SC_GETPW_R_SIZE_MAX);
    assert_eq!(_CS_DARWIN_USER_TEMP_DIR, libc::_CS_DARWIN_USER_TEMP_DIR);
    assert_eq!(AT_FDCWD, libc::AT_FDCWD);
    assert_eq!(AT_SYMLINK_NOFOLLOW, libc::AT_SYMLINK_NOFOLLOW);
    assert_eq!(AT_SYMLINK_FOLLOW, libc::AT_SYMLINK_FOLLOW);
    assert_eq!(AT_REMOVEDIR, libc::AT_REMOVEDIR);
    assert_eq!(KERN_SUCCESS, libc::KERN_SUCCESS);
    assert_eq!(RUSAGE_INFO_V4, libc::RUSAGE_INFO_V4);
}

#[cfg(target_os = "macos")]
#[test]
fn the_signatures_are_libcs() {
    // Each declaration as a pointer of the C type, and libc's as the same pointer: a type that
    // differs fails to compile here. libc's macOS `pthread_t` is a `uintptr_t`; ours is the
    // header's pointer, so the calls that take one are not compared.
    let _: [unsafe extern "C" fn(pthread_key_t) -> c_int; 2] = [pthread_key_delete, libc::pthread_key_delete];
    let _: [unsafe extern "C" fn(pthread_key_t) -> *mut c_void; 2] = [pthread_getspecific, libc::pthread_getspecific];
    let _: [unsafe extern "C" fn(pthread_key_t, *const c_void) -> c_int; 2] = [pthread_setspecific, libc::pthread_setspecific];
    let _: [unsafe extern "C" fn(c_int) -> c_long; 2] = [sysconf, libc::sysconf];
    let _: [unsafe extern "C" fn() -> *mut c_char; 2] = [dlerror, libc::dlerror];
    let _: [unsafe extern "C" fn(*mut c_void) -> c_int; 2] = [dlclose, libc::dlclose];
    let _: [unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void; 2] = [dlopen, libc::dlopen];
    let _: [unsafe extern "C" fn() -> crate::posix::types::pid_t; 2] = [getppid, libc::getppid];
    let _: [unsafe extern "C" fn() -> crate::posix::types::pid_t; 2] = [setsid, libc::setsid];
    let _: [unsafe extern "C" fn() -> crate::posix::types::uid_t; 2] = [getuid, libc::getuid];
}

#[test]
fn the_calls_agree_with_libcs() {
    // SAFETY: each libc call takes no memory, or a live local it writes.
    unsafe {
        assert_eq!(getppid(), libc::getppid());
        assert_eq!(getuid(), libc::getuid());
        assert_eq!(geteuid(), libc::geteuid());
        assert_eq!(getpid(), libc::getpid());

        let mut theirs: libc::rlimit = core::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut theirs), 0);
        let ours = resource_limit(RLIMIT_NOFILE).unwrap();
        assert_eq!((ours.rlim_cur, ours.rlim_max), (theirs.rlim_cur, theirs.rlim_max));

        let mut theirs: libc::utsname = core::mem::zeroed();
        assert_eq!(libc::uname(&raw mut theirs), 0);
        let ours = system_name().unwrap();
        // Linux's fields are bytes, libc's `c_char` (signed on x86-64): compared as bytes.
        assert_eq!(ours.sysname.map(|c| c as u8), theirs.sysname.map(|c| c as u8));
        assert_eq!(ours.release.map(|c| c as u8), theirs.release.map(|c| c as u8));
        assert_eq!(ours.machine.map(|c| c as u8), theirs.machine.map(|c| c as u8));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_linux_calls_agree_with_libcs() {
    use crate::Errno;
    use crate::linux::host;
    // SAFETY: each libc call takes numbers and no memory.
    unsafe {
        assert_eq!(host::getpgid(0), Ok(libc::getpgid(0)));
    }
    // A negative group is refused before anything changes, the same as glibc's `setpgid`.
    assert_eq!(set_process_group(0, -1), Err(Errno::Inval));
    // A limit set to what it already is changes nothing and reads back the same.
    let now = host::prlimit64(0, host::Resource::NOFILE, None).unwrap();
    assert_eq!(host::prlimit64(0, host::Resource::NOFILE, Some(&now)), Ok(now));
    assert_eq!(resource_limit(-1), Err(Errno::Inval));
}

#[cfg(target_os = "macos")]
#[test]
fn a_key_holds_this_threads_value() {
    let key = key_create(None).unwrap();
    let value = 0x5eed_usize as *const c_void;
    // SAFETY: `key` is live, and has no destructor to be called with `value`.
    unsafe { set_specific(key, value).unwrap() };
    // SAFETY: as above.
    assert_eq!(unsafe { pthread_getspecific(key) }.cast_const(), value);
    std::thread::spawn(move || {
        // SAFETY: as above; another thread's value under the key is its own, null.
        assert!(unsafe { pthread_getspecific(key) }.is_null());
    })
    .join()
    .unwrap();
    key_delete(key).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn the_stack_holds_a_local() {
    let local = 0u8;
    let at = (&raw const local).addr();
    let stack = this_thread_stack().unwrap();
    assert!(stack.low < at && at < stack.high, "{at:#x} outside {stack:x?}");
}

#[test]
fn the_thread_id_is_the_systems() {
    #[cfg(target_os = "linux")]
    // SAFETY: `gettid` takes nothing and cannot fail.
    let theirs = unsafe { libc::gettid() } as u64;
    #[cfg(target_os = "macos")]
    let theirs = {
        let mut id = 0u64;
        // SAFETY: a null thread is the calling one; `id` is a live local.
        assert_eq!(unsafe { libc::pthread_threadid_np(0, &raw mut id) }, 0);
        id
    };
    assert_eq!(thread_id(), theirs);
}

#[cfg(target_os = "macos")]
#[test]
fn the_loader_finds_what_is_loaded() {
    // SAFETY: `RTLD_DEFAULT` is a handle every system has.
    let found = unsafe { symbol(RTLD_DEFAULT, c"getpid") }.expect("getpid in some image");
    let info = address_info(found.as_ptr()).expect("getpid's address in an image");
    assert!(!info.dli_fname.is_null());
    // SAFETY: as above.
    assert!(unsafe { symbol(RTLD_DEFAULT, c"blaze_sys_no_such_symbol") }.is_none());
    let here = address_info(the_loader_finds_what_is_loaded as *const c_void).expect("this test's own image");
    assert!(!here.dli_fbase.is_null());
}

#[cfg(target_os = "linux")]
#[test]
fn the_affinity_is_libcs() {
    let ours = affinity().unwrap();
    assert!(ours.count() > 0);
    // SAFETY: `theirs` is a live `cpu_set_t` of the size given.
    let theirs = unsafe {
        let mut theirs: libc::cpu_set_t = core::mem::zeroed();
        assert_eq!(libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut theirs), 0);
        theirs
    };
    for cpu in 0..CPU_SETSIZE {
        assert_eq!(ours.contains(cpu), libc::CPU_ISSET(cpu, &theirs), "cpu {cpu}");
    }
    assert_eq!(ours.count(), libc::CPU_COUNT(&theirs) as u32);
    // Setting the set it has changes nothing.
    set_affinity(&ours).unwrap();
    assert_eq!(affinity().unwrap(), ours);
    let mut set = cpu_set_t::EMPTY;
    assert!(set.insert(3));
    assert!(!set.insert(CPU_SETSIZE));
    assert_eq!(set.count(), 1);
    assert!(set.contains(3) && !set.contains(2));
}

#[cfg(target_os = "macos")]
#[test]
fn the_task_answers_are_the_kernels() {
    // SAFETY: libc's reads the same `mach_task_self_`, set before `main`.
    assert_eq!(mach_task_self(), unsafe { libc::mach_task_self() });
    let task = proc_task_info(getpid()).unwrap();
    assert!(task.pti_resident_size > 0);
    #[cfg(target_arch = "aarch64")]
    {
        let basic = task_basic_info_self().unwrap();
        assert!({ basic.resident_size } > 0);
        // SAFETY: the calling thread's port, for this task.
        let me = unsafe { pthread_mach_thread_np(pthread_self()) };
        let thread = thread_basic_info_of(me).unwrap();
        assert_eq!(thread.suspend_count, 0);
    }
    let mut buf = [0u8; 1024];
    assert!(sysctl_bytes(c"kern.ostype", &mut buf).unwrap().is_some_and(|len| len > 1));
    assert_eq!(sysctl_bytes(c"blaze.no.such.name", &mut buf).unwrap(), None);
}
