//! The sandbox on Windows: an AppContainer in a job object ([#1390]). The launcher's own process
//! stays outside: it creates the program's process in the container and, from its creation, in a
//! job that holds it and everything it starts, waits for it and takes its write grants back.
//!
//! A process in an AppContainer opens a file only where the file's DACL lets its user and also
//! the container's SID, a capability SID the process holds, or ALL APPLICATION PACKAGES (the
//! system's own directories) open it. The container is one profile, [`PROFILE`], made once; the
//! reads a spec names are granted to its SID, inheritably, and kept, so each directory is granted
//! once. Writes are granted for one run to a capability SID of the run's own, derived from a
//! random name no other process holds, and taken back as the run ends: a grant a killed launcher
//! leaves behind names a SID nothing holds again. A directory the launcher makes (the run's
//! temporary directory, a missing writable one) is made with its grant in it, so no DACL is read
//! and rewritten for it; the temporary directory is removed as the run ends, grant and all. There
//! is no network without the internet and
//! private-network capabilities, which only a spec with `network` is given. What is read is not
//! traced on Windows: the caller says so in the record.
//!
//! A run is started by `blaze sandbox --v2`, one process a run, or by the warm launcher
//! (`blaze sandbox --v2-serve`, [`serve`]), one process a daemon, which runs each on a thread of
//! its own with the standard handles and the job its driver handed it ([`Given`]). What does not
//! change from run to run is made once a process: the container's and the well-known SIDs
//! ([`Kept`]) and the session lock's handle; in the warm launcher also the read checks
//! ([`reads`]).
//!
//! This module and [`reads`] and [`serve`] below it are the Windows driver, the one place the
//! sandbox may use unsafe code on Windows: std has no AppContainer, job object, DACL, token or
//! change-watch call, so each is a `windows-sys` call in a block of its own with what it relies
//! on stated above it. The read checks' table ([`reads`]) is lock-free so that the warm
//! launcher's runs look a path up without a lock they all take; a `Mutex<HashMap>` would be the
//! safe version, at one shared lock a check. Its model tests run with `--cfg blaze_check`.
//!
//! [#1390]: https://github.com/indexable-inc/blaze/issues/1390

mod reads;
pub mod serve;

use std::{
    ffi::{OsStr, c_void},
    fmt::{Display, Write as _},
    io::ErrorKind,
    marker::PhantomData,
    os::windows::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::OnceLock,
    time::{Duration, Instant},
};

use blaze_usercode::confine::Spec;
use parking_lot::{Condvar, Mutex};
use windows_sys::{
    Win32::{
        Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS,
            ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GENERIC_ALL, GENERIC_EXECUTE,
            ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
            INVALID_HANDLE_VALUE, LocalFree, STATUS_CONTROL_C_EXIT, WAIT_ABANDONED, WAIT_FAILED,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
            AddAccessAllowedAceEx,
            Authorization::{
                EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo,
                NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW,
                SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
            },
            CONTAINER_INHERIT_ACE, CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
            DeriveCapabilitySidsFromName, EqualSid, FreeSid, GetAce, GetAclInformation, GetLengthSid,
            INHERIT_ONLY_ACE, InitializeAcl, InitializeSecurityDescriptor,
            Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName},
            NO_INHERITANCE, NO_PROPAGATE_INHERIT_ACE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
            PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SECURITY_DESCRIPTOR,
            SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            SetSecurityDescriptorDacl, WELL_KNOWN_SID_TYPE, WinBuiltinAnyPackageSid,
            WinCapabilityInternetClientServerSid, WinCapabilityInternetClientSid,
            WinCapabilityPrivateNetworkClientServerSid,
        },
        Storage::FileSystem::{
            CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
            GetFileAttributesW, GetFileInformationByHandleEx, INVALID_FILE_ATTRIBUTES,
            OPEN_EXISTING, READ_CONTROL,
        },
        System::{
            Console::{
                CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, GetStdHandle, STD_ERROR_HANDLE,
                STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
            },
            JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
                JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
                JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
                JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
                JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_BASIC_UI_RESTRICTIONS,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOBOBJECTINFOCLASS, IsProcessInJob,
                JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
                JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_ENABLED, SECURITY_DESCRIPTOR_REVISION},
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
                GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
                LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                ReleaseMutex, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
                UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
            },
        },
    },
    core::{BOOL, HRESULT},
};

/// The AppContainer profile every run is in.
const PROFILE: &str = "blaze.sandbox";

/// What a read grant gives: open, list, read and run.
const READ: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// What a write grant gives.
const WRITE: u32 = FILE_ALL_ACCESS;

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`: the profile was made before.
const PROFILE_EXISTS: HRESULT = (0x8007_0000 | ERROR_ALREADY_EXISTS) as HRESULT;

/// The lock every launcher of the session holds while it changes a DACL: a change reads the
/// DACL and writes it whole, and two at once would lose one's entry.
const LOCK: &str = "Local\\blaze.sandbox.acl";

/// How long a launcher waits for the processes of its job to be gone once it ended them.
const ENDING: Duration = Duration::from_secs(5);

/// A SID of the system's: as long as the longest, and aligned as a SID's fields are.
type SidBuffer = [u32; (SECURITY_MAX_SID_SIZE / 4) as usize];

/// What the read grants are asked about and made for.
const GRANTING_READ: &str = "granting the sandbox read";

/// The limits a job a driver hands a run must have already: what is left of the run ends with
/// its job's last handle, and a crash ends a process rather than waiting on a dialog.
const HANDED_LIMITS: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;

/// Where a run's standard handles and job come from.
pub struct Given {
    /// Whether a driver handed them to the warm launcher; otherwise they are the launcher's own.
    driver: bool,
    standard: [Option<Handle>; 3],
    job: Option<Handle>,
}

// SAFETY: a `Given`'s handles are its own ([`Given::adopt`]), named by no other value; it is
// made on the thread that reads the frame and moved, whole, to the one thread that runs it, and
// a kernel handle may be used and closed from any thread.
unsafe impl Send for Given {}

impl Given {
    /// The launcher's own standard handles, copied for the program, and a job of the run's own:
    /// `blaze sandbox --v2`, a process a run.
    pub fn own() -> Given {
        Given { driver: false, standard: [None, None, None], job: None }
    }

    /// The handles a driver duplicated into the warm launcher for one run
    /// ([`blaze_core::Handed`]): the program's standard input, output and error (none for a 0),
    /// inheritable, and the job it runs in, which the driver made, kills and reads the memory of.
    ///
    /// # Safety
    ///
    /// Each value of `handed` that is not 0 is a handle of this process the driver made for this
    /// run alone, named in no other frame and not closed: from here on it is the `Given`'s, and
    /// closed once, as it drops. A value named twice would be closed twice, and the second close
    /// could close whatever reused its number.
    pub unsafe fn adopt(handed: &blaze_core::Handed) -> Given {
        let owned = |value: u64| (value != 0).then_some(Handle(value as usize as HANDLE));
        Given {
            driver: true,
            standard: [owned(handed.stdin), owned(handed.stdout), owned(handed.stderr)],
            job: owned(handed.job),
        }
    }
}

/// Runs `spec.program` with `spec.args`, exactly `spec.env` (plus TMP and TEMP pointing at the
/// run's own temporary directory), in `spec.dir`, sandboxed, with the standard handles and job
/// `given` says: what it ends with, its exit code. `Err(why)` when no sandbox could be made, and
/// then nothing ran.
pub fn run(spec: &Spec, given: Given) -> Result<i32, String> {
    let _running = Running::start();
    on_interrupt()?;
    let mut made = make(spec.network, given)?;
    let temp = temporary(spec)?;
    let writable = Inheritable::new(made.run.sid(), WRITE)?;
    match writable.make_dir(&temp) {
        Ok(()) => {}
        // Left by a launcher that was killed: the scheduler runs a spec once at a time.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            std::fs::remove_dir_all(&temp).map_err(|error| {
                // alloc: the error, once, and nothing runs.
                format!("removing a killed run's temporary directory {}: {error}", temp.display())
            })?;
            writable.make_dir(&temp).map_err(|error| {
                // alloc: the error, once, and nothing runs.
                format!("making the run's temporary directory {}: {error}", temp.display())
            })?;
        }
        // alloc: the error, once, and nothing runs.
        Err(error) => return Err(format!("making the run's temporary directory {}: {error}", temp.display())),
    }
    let writes = Vec::with_capacity(spec.write.len());
    // clone: once per sandbox, the directory it takes its grant back from.
    let mut granted = Granted { sid: made.run.sid(), writes, temp: temp.clone() };
    let mut found = Vec::with_capacity(spec.write.len());
    for dir in &spec.write {
        match writable.make_dir_all(dir) {
            // Made here with the grant in it; it stays, so the grant is taken back at the end.
            // clone: once per sandbox and directory, as above.
            Ok(true) => granted.writes.push(dir.clone()),
            Ok(false) => found.push(dir.as_path()),
            Err(error) => {
                // alloc: the error, once, and nothing runs.
                return Err(format!("making the sandbox's writable directory {}: {error}", dir.display()));
            }
        }
    }
    let program_dir = spec.program.parent().filter(|it| !it.as_os_str().is_empty());
    let readers = [sid(&made.kept.container), sid(&made.kept.any)];
    let checks = reads::kept();
    // Once per sandbox, the key each read path is looked up by in the kept checks.
    let mut key = Vec::with_capacity(if checks.is_some() { 260 } else { 0 });
    let mut missing = Vec::with_capacity(spec.read.len() + 2);
    for path in spec.read.iter().map(PathBuf::as_path).chain(program_dir).chain([spec.dir.as_path()]) {
        let seen = match checks {
            Some(checks) => checks.covered(path, &readers, &mut key)?,
            None => covered(path, &readers, READ, GRANTING_READ)?,
        };
        let Some(seen) = seen else { continue };
        // Read grants are only ever added, so one seen without the lock stays: only a missing
        // one is changed, under the lock, after looking again.
        if !seen.covered {
            missing.push((path, seen.dir));
        }
    }
    if !missing.is_empty() || !found.is_empty() {
        let _lock = Lock::take()?;
        for (path, dir) in missing {
            grant(path, dir, readers[0], &readers, READ, GRANTING_READ)?;
        }
        for dir in found {
            if grant(dir, true, granted.sid, &[granted.sid], WRITE, "granting the sandbox write")? {
                // alloc: once per sandbox and directory, the grant to take back.
                granted.writes.push(dir.to_owned());
            }
        }
    }

    // The program's environment, once per sandbox, for the kernel.
    let mut env = Vec::with_capacity(spec.env.len() + 2);
    env.extend(spec.env.iter().cloned());
    // clone: as above.
    env.push(("TMP".into(), temp.clone().into_os_string()));
    env.push(("TEMP".into(), temp.into_os_string()));
    let block = blaze_core::create_process::environment(&env).map_err(|it| it.to_string())?; // alloc: error path
    let mut line =
        blaze_core::create_process::command_line(&spec.program, &spec.args).map_err(|it| it.to_string())?; // alloc: error path
    let program = wide(spec.program.as_os_str(), "the program's path")?;
    let dir = wide(spec.dir.as_os_str(), "the program's directory")?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let standard = |at: usize| made.standard[at].as_ref().map_or(null_mut(), |it| it.0);
    startup.StartupInfo.hStdInput = standard(0);
    startup.StartupInfo.hStdOutput = standard(1);
    startup.StartupInfo.hStdError = standard(2);
    startup.lpAttributeList = made.attributes.list();
    let inherit = BOOL::from(!made.inherited.is_empty());
    let mut process = PROCESS_INFORMATION::default();
    // Before the process exists, so an interrupt while it is made ends it too.
    let _registered = Registered::new(&made.job);
    // SAFETY: `program` and `dir` end in NUL (`wide`), `line` ends in NUL and is this frame's
    // own buffer, which CreateProcessW may write into (`command_line`), and `block` is a UTF-16
    // environment ending in two NULs (`environment`), as CREATE_UNICODE_ENVIRONMENT says. The
    // attribute list and the capabilities, SIDs and handles it points into (the job's among
    // them) are in `made`, on the heap and live until `run` returns ([`Made`]). `startup` is an
    // STARTUPINFOEXW whose `cb` says so, as EXTENDED_STARTUPINFO_PRESENT has Windows read it;
    // `process` is a live PROCESS_INFORMATION for it to fill. A string without its NUL would be
    // read past its end.
    let created = unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            inherit,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            block.as_ptr().cast(),
            dir.as_ptr(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut process,
        )
    };
    if created == 0 {
        // alloc: the error, once, and nothing ran.
        return Err(failed(format!("starting {} in the sandbox", spec.program.display()), "CreateProcessW"));
    }
    let child = Handle(process.hProcess);
    drop(Handle(process.hThread));
    // SAFETY: `child` owns the process handle until `run` returns, so the wait is on this
    // process and no other object that took its number.
    if unsafe { WaitForSingleObject(child.0, INFINITE) } != WAIT_OBJECT_0 {
        let why = failed("waiting for the program, which started", "WaitForSingleObject");
        end(&made.job, 1);
        return Err(why);
    }
    let mut code = 0u32;
    // SAFETY: `child` still owns the process handle, and `code` is a live `u32` for the call to
    // write and nothing else.
    if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
        let why = failed("taking the exit code of the program, which ran", "GetExitCodeProcess");
        end(&made.job, 1);
        return Err(why);
    }
    end(&made.job, code);
    Ok(code as i32)
}

/// Makes a sandbox around nothing (no program run): `Ok` when one can be made here, why not
/// otherwise.
pub fn probe() -> Result<(), String> {
    let _made = make(true, Given::own())?;
    let _lock = Lock::take()?;
    Ok(())
}

/// What a run is made of up to its process: the run's capability, the job and the process's
/// attribute list. The list points into the capabilities, the SIDs and the handles beside it,
/// which are on the heap, where moving `Made` leaves them, or in [`Kept`], which lives as long
/// as the process.
struct Made {
    attributes: Attributes,
    _capabilities: Box<SECURITY_CAPABILITIES>,
    _granted: Vec<SID_AND_ATTRIBUTES>,
    _jobs: Box<[HANDLE; 1]>,
    kept: &'static Kept,
    run: Capability,
    job: Handle,
    standard: [Option<Handle>; 3],
    inherited: Vec<HANDLE>,
}

/// The SIDs every run of the process shares, made by its first and kept: the container's, ALL
/// APPLICATION PACKAGES' and the three network capabilities'. Copied into buffers of their own,
/// so a static holds plain bytes.
struct Kept {
    container: SidBuffer,
    any: SidBuffer,
    network: [SidBuffer; 3],
}

static KEPT: OnceLock<Kept> = OnceLock::new();

/// The process's [`Kept`], made the first time; an error is not kept, so a later run asks again.
fn kept() -> Result<&'static Kept, String> {
    if let Some(kept) = KEPT.get() {
        return Ok(kept);
    }
    let container = Container::open()?;
    let made = Kept {
        container: copied(container.0, "the sandbox's AppContainer")?,
        any: well_known(WinBuiltinAnyPackageSid, "ALL APPLICATION PACKAGES")?,
        network: [
            well_known(WinCapabilityInternetClientSid, "the internetClient capability")?,
            well_known(WinCapabilityInternetClientServerSid, "the internetClientServer capability")?,
            well_known(WinCapabilityPrivateNetworkClientServerSid, "the privateNetworkClientServer capability")?,
        ],
    };
    Ok(KEPT.get_or_init(|| made))
}

/// A copy of `sid` in a buffer of its own.
fn copied(sid: PSID, what: &str) -> Result<SidBuffer, String> {
    let mut buffer: SidBuffer = [0; (SECURITY_MAX_SID_SIZE / 4) as usize];
    // SAFETY: `buffer` is SECURITY_MAX_SID_SIZE bytes, the length the call is told, live and
    // exclusive for it and aligned for a SID's `u32` fields; `sid` is a valid SID its caller
    // holds for the call ([`Container`]), which only reads it.
    if unsafe { CopySid(SECURITY_MAX_SID_SIZE, buffer.as_mut_ptr().cast(), sid) } == 0 {
        // alloc: the error, once, and the sandbox stops.
        return Err(failed(format!("copying the SID of {what}"), "CopySid"));
    }
    Ok(buffer)
}

fn make(network: bool, given: Given) -> Result<Made, String> {
    let kept = kept()?;
    let run = Capability::new()?;
    let enabled = SE_GROUP_ENABLED as u32;
    let mut granted = Vec::with_capacity(1 + kept.network.len());
    granted.push(SID_AND_ATTRIBUTES { Sid: run.sid(), Attributes: enabled });
    if network {
        granted.extend(kept.network.iter().map(|it| SID_AND_ATTRIBUTES { Sid: sid(it), Attributes: enabled }));
    }
    // alloc: once per sandbox, where the attribute list's pointers into it stay put ([`Made`]).
    let capabilities = Box::new(SECURITY_CAPABILITIES {
        AppContainerSid: sid(&kept.container),
        Capabilities: granted.as_mut_ptr(),
        CapabilityCount: granted.len() as u32,
        Reserved: 0,
    });
    let Given { driver, standard: handed, job: handed_job } = given;
    let (job, standard) = match (driver, handed_job) {
        (false, _) => (job()?, standard()?),
        (true, Some(theirs)) => (driven(theirs)?, handed),
        // alloc: the error, once, and nothing runs.
        (true, None) => return Err("the driver handed the run no job to hold it in".to_owned()),
    };
    // alloc: as `capabilities` is.
    let jobs = Box::new([job.0]);
    let mut inherited: Vec<HANDLE> = Vec::with_capacity(3);
    inherited.extend(standard.iter().flatten().map(|it| it.0));
    let mut attributes = Attributes::new(if inherited.is_empty() { 2 } else { 3 })?;
    // SAFETY: `capabilities` is one SECURITY_CAPABILITIES in a box, which `Made` keeps beside
    // `attributes` (moving `Made` leaves it where it is), and what it points to is kept there
    // too (`granted`'s list and `run`'s SID) or in `kept`, a static (the container's and the
    // network's SIDs).
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            (&raw const *capabilities).cast(),
            size_of::<SECURITY_CAPABILITIES>(),
            "putting the program in the sandbox's AppContainer",
        )
    }?;
    // In the job from its creation, so it runs nothing outside it and needs no suspended start,
    // assignment and resume.
    // SAFETY: `jobs` is one HANDLE in a box, which `Made` keeps beside `attributes` and never
    // changes; the handle is `job`'s, also kept there, open until `Made` drops.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            size_of::<HANDLE>(),
            "holding the program in the sandbox's job",
        )
    }?;
    if !inherited.is_empty() {
        // SAFETY: `inherited` is `len` handles on the heap, which `Made` keeps beside
        // `attributes` and never changes; the handles are `standard`'s, also kept there.
        unsafe {
            attributes.set(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                inherited.as_ptr().cast(),
                inherited.len() * size_of::<HANDLE>(),
                "passing the program its standard handles and no other",
            )
        }?;
    }
    Ok(Made { attributes, _capabilities: capabilities, _granted: granted, _jobs: jobs, kept, run, job, standard, inherited })
}

fn failed(what: impl Display, call: &str) -> String {
    // alloc: the error, once, and the sandbox stops.
    format!("{what} ({call}): {}", std::io::Error::last_os_error())
}

fn failed_with(what: impl Display, call: &str, code: i32) -> String {
    // alloc: the error, once, and the sandbox stops.
    format!("{what} ({call}): {}", std::io::Error::from_raw_os_error(code))
}

/// The run's temporary directory, which the program sees as TMP and TEMP: named by what the spec
/// is, so the same spec sees the same path.
fn temporary(spec: &Spec) -> Result<PathBuf, String> {
    // alloc: the error, once, and nothing runs.
    let bytes = serde_json::to_vec(spec).map_err(|error| format!("the spec: {error}"))?;
    let digest = blaze_ids::b3::hash(&bytes);
    let mut name = String::with_capacity(16);
    for byte in &digest[..8] {
        _ = write!(name, "{byte:02x}");
    }
    // alloc: once per sandbox, its temporary directory's path.
    Ok(std::env::temp_dir().join(format!("blaze-sandbox-{name}")))
}

/// `text` as UTF-16 ending in NUL, or why not when it has a NUL of its own.
fn wide(text: &OsStr, what: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend(text.encode_wide());
    if out.contains(&0) {
        // alloc: the error, once, and the sandbox stops.
        return Err(format!("{what} has a NUL in it: {}", text.to_string_lossy()));
    }
    out.push(0);
    Ok(out)
}

fn literal(text: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.extend(text.encode_utf16().chain([0]));
    out
}

fn sid(buffer: &SidBuffer) -> PSID {
    buffer.as_ptr().cast_mut().cast()
}

/// A handle, closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: every `Handle` is made from a handle a call just returned to this module
            // (CreateProcessW, DuplicateHandle, CreateJobObjectW, CreateFileW, OpenProcess), never copied
            // into another `Handle`, so this is its one close; closing it twice could close
            // whatever reused its number.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The SID of the [`PROFILE`] container, made the first time.
struct Container(PSID);

impl Container {
    fn open() -> Result<Container, String> {
        let name = literal(PROFILE);
        let shown = literal("blaze sandbox");
        let mut sid: PSID = null_mut();
        // The folder CreateAppContainerProfile makes: while it is there the profile is, and the
        // SID is derived with no registry write. Without it, or without `LOCALAPPDATA` to find
        // it by, the profile is made (or found) as on a first run, never assumed.
        let made_already = std::env::var_os("LOCALAPPDATA")
            .is_some_and(|local| directory(&Path::new(&local).join("Packages").join(PROFILE)));
        let made = match made_already {
            true => PROFILE_EXISTS,
            // SAFETY: `name` and `shown` end in NUL (`literal`) and live in this frame for the
            // call, which only reads them; a null list of 0 capabilities is read as none; `sid`
            // is a live PSID for it to set, freed by `Container`'s drop.
            false => unsafe {
                CreateAppContainerProfile(name.as_ptr(), shown.as_ptr(), shown.as_ptr(), null(), 0, &mut sid)
            },
        };
        if made == PROFILE_EXISTS {
            // SAFETY: `name` ends in NUL (`literal`) and lives in this frame; `sid`, still null
            // as CreateAppContainerProfile failed, is a live PSID for the call to set.
            let found = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
            if found < 0 {
                return Err(failed_with(
                    // alloc: the error, once, and the sandbox stops.
                    format!("finding the sandbox's AppContainer profile {PROFILE}"),
                    "DeriveAppContainerSidFromAppContainerName",
                    found,
                ));
            }
        } else if made < 0 {
            return Err(failed_with(
                // alloc: the error, once, and the sandbox stops.
                format!("making the sandbox's AppContainer profile {PROFILE}"),
                "CreateAppContainerProfile",
                made,
            ));
        }
        Ok(Container(sid))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is the SID CreateAppContainerProfile or
            // DeriveAppContainerSidFromAppContainerName made (`open`), which both say to free
            // with FreeSid, and `Container` is never copied, so this frees it once.
            unsafe { FreeSid(self.0) };
        }
    }
}

/// The capability SID of one run, derived from a random name.
struct Capability {
    groups: *mut PSID,
    group_count: u32,
    sids: *mut PSID,
    count: u32,
}

impl Capability {
    fn new() -> Result<Capability, String> {
        // alloc: once per sandbox, the name its capability is derived from.
        let name = literal(&format!("blaze.sandbox.run.{:032x}", std::random::random::<u128>(..)));
        let mut it = Capability { groups: null_mut(), group_count: 0, sids: null_mut(), count: 0 };
        // SAFETY: `name` ends in NUL (`literal`) and lives in this frame; the four out-pointers
        // are live fields of `it`, and the lists the call makes there are freed only by
        // `Capability`'s drop, which reads `count`s the same call set.
        let derived = unsafe {
            DeriveCapabilitySidsFromName(
                name.as_ptr(),
                &mut it.groups,
                &mut it.group_count,
                &mut it.sids,
                &mut it.count,
            )
        };
        if derived == 0 {
            return Err(failed("deriving the run's capability SID", "DeriveCapabilitySidsFromName"));
        }
        if it.count == 0 || it.sids.is_null() {
            return Err("deriving the run's capability SID (DeriveCapabilitySidsFromName): it gave none".into());
        }
        Ok(it)
    }

    fn sid(&self) -> PSID {
        // SAFETY: `new` returns a `Capability` only when `sids` is not null and `count` is at
        // least 1, so the first entry is in the list DeriveCapabilitySidsFromName made, which
        // lives until `self` drops; with none, this would read past the list.
        unsafe { *self.sids }
    }
}

impl Drop for Capability {
    fn drop(&mut self) {
        for (list, count) in [(self.groups, self.group_count), (self.sids, self.count)] {
            if list.is_null() {
                continue;
            }
            for at in 0..count as usize {
                // SAFETY: `list` is not null, so DeriveCapabilitySidsFromName made it with
                // `count` entries (`new`), and `at < count`: the offset stays inside it.
                let entry = unsafe { list.add(at) };
                // SAFETY: `entry` is inside the list (above), which is not freed until after
                // this loop, and every entry the call wrote is an initialized PSID.
                let sid = unsafe { *entry };
                // SAFETY: each SID of the list was made by LocalAlloc in
                // DeriveCapabilitySidsFromName, which says to free it with LocalFree, and this
                // drop, the one for a `Capability` that is never copied, frees it once.
                unsafe { LocalFree(sid) };
            }
            // SAFETY: the list itself was made by LocalAlloc too, its SIDs are freed above and
            // nothing reads it after; this drop runs once.
            unsafe { LocalFree(list.cast()) };
        }
    }
}

fn well_known(kind: WELL_KNOWN_SID_TYPE, what: &str) -> Result<SidBuffer, String> {
    let mut buffer: SidBuffer = [0; (SECURITY_MAX_SID_SIZE / 4) as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: `buffer` is SECURITY_MAX_SID_SIZE bytes, the `size` the call is told, live and
    // exclusive for it, and aligned for a SID's `u32` fields ([`SidBuffer`]); the call writes at
    // most `size` bytes and sets `size` to what it wrote. A null domain is the local one.
    if unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut size) } == 0 {
        // alloc: the error, once, and the sandbox stops.
        return Err(failed(format!("making the SID of {what}"), "CreateWellKnownSid"));
    }
    Ok(buffer)
}

/// The job a run's processes are held in: killed when it is closed, a crash ending a process
/// rather than waiting on a dialog, no way out for a child, and none of the desktop's shared
/// state (the clipboard, atoms, other processes' windows, the display and system settings,
/// logging off).
fn job() -> Result<Handle, String> {
    // SAFETY: both arguments may be null, for the default security and no name; the call reads
    // no memory of this process. The handle it gives, or null, is owned by the `Handle`.
    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(failed("making the sandbox's job", "CreateJobObjectW"));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = HANDED_LIMITS;
    // SAFETY: JobObjectExtendedLimitInformation names a JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    unsafe { set_job(&job, JobObjectExtendedLimitInformation, &limits, "limiting the sandbox's job") }?;
    off_the_desktop(&job)?;
    Ok(job)
}

/// The job a driver handed the run, which made it with [`HANDED_LIMITS`] and its memory limit:
/// checked, not set, so its limits stay the driver's, and kept off the desktop as [`job`] is.
fn driven(job: Handle) -> Result<Handle, String> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: JobObjectExtendedLimitInformation names a JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    // and `limits` is a live, exclusive one of the size the call is told, which it writes and
    // nothing more; a null length out-pointer is allowed. `job` owns the handle the driver
    // duplicated in ([`Given::adopt`]).
    let asked = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    };
    if asked == 0 {
        return Err(failed("reading the limits of the job the driver handed", "QueryInformationJobObject"));
    }
    if limits.BasicLimitInformation.LimitFlags & HANDED_LIMITS != HANDED_LIMITS {
        // alloc: the error, once, and nothing runs.
        return Err(format!(
            "the job the driver handed has limits {:#x}, without killing what is left on its close and ending a \
             process that crashes ({HANDED_LIMITS:#x})",
            limits.BasicLimitInformation.LimitFlags
        ));
    }
    off_the_desktop(&job)?;
    Ok(job)
}

/// None of the desktop's shared state for `job`'s processes.
fn off_the_desktop(job: &Handle) -> Result<(), String> {
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
    };
    // SAFETY: JobObjectBasicUIRestrictions names a JOBOBJECT_BASIC_UI_RESTRICTIONS.
    unsafe { set_job(job, JobObjectBasicUIRestrictions, &ui, "keeping the sandbox's job off the desktop") }
}

/// # Safety
///
/// `T` is the structure `class` names: the call reads `info` as that structure, and one of
/// another layout would be read wrongly or past its end.
unsafe fn set_job<T>(job: &Handle, class: JOBOBJECTINFOCLASS, info: &T, what: &str) -> Result<(), String> {
    // SAFETY: `info` is a live `T` of `size_of::<T>()` bytes, read only, and `T` is what `class`
    // names (`# Safety`); `job` owns an open job handle.
    let set = unsafe { SetInformationJobObject(job.0, class, (info as *const T).cast(), size_of::<T>() as u32) };
    match set {
        0 => Err(failed(what, "SetInformationJobObject")),
        _ => Ok(()),
    }
}

/// Ends every process of `job` with `code` and waits, at most [`ENDING`], for them to be gone,
/// so what they had open is closed before the run's directories are taken back.
fn end(job: &Handle, code: u32) {
    if listed(job).is_none_or(|(_, count)| count == 0) {
        // The usual run, whose program started nothing that outlived it, costs this one call.
        return;
    }
    // SAFETY: `job` owns an open job handle, borrowed for the call; the call reads no memory
    // of this process.
    unsafe { TerminateJobObject(job.0, code) };
    if let Err(left) = gone_within(job, ENDING) {
        eprintln!("blaze: {left} processes of the sandbox's job did not end within {ENDING:?}");
    }
}

/// Room for this many process numbers in one ask of the job's list: past it the ask is
/// refused with `ERROR_MORE_DATA` and lists the first, which [`gone_within`] waits for before
/// it asks again.
const LISTED: usize = 256;

/// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for [`LISTED`] numbers: the structure the system
/// writes is its two counts then as many numbers as fit.
#[repr(C)]
struct Listed {
    head: JOBOBJECT_BASIC_PROCESS_ID_LIST,
    more: [usize; LISTED - 1],
}

/// The numbers of the processes in `job`, as many as fit, and how many it holds; `None` when
/// the job could not be asked.
fn listed(job: &Handle) -> Option<(Box<Listed>, usize)> {
    // SAFETY: all zeros is a list of nothing.
    // alloc: once per ended run that started something, 2 KiB off the launcher's stack.
    let mut list: Box<Listed> = Box::new(unsafe { std::mem::zeroed() });
    // SAFETY: JobObjectBasicProcessIdList names a JOBOBJECT_BASIC_PROCESS_ID_LIST, whose list
    // runs on past its declared one entry into `more`: `list` is a live, exclusive buffer of the
    // size the call is told, which it writes and nothing more; a null length out-pointer is
    // allowed. `job` owns an open job handle.
    let asked = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicProcessIdList,
            (&raw mut *list).cast(),
            size_of::<Listed>() as u32,
            null_mut(),
        )
    };
    // SAFETY: reads the calling thread's last error, nothing else.
    if asked == 0 && unsafe { GetLastError() } != ERROR_MORE_DATA {
        return None;
    }
    let count = (list.head.NumberOfProcessIdsInList as usize).min(LISTED);
    Some((list, count))
}

/// Waits until every process in `job` is gone, at most `long` in all: on each one's own
/// handle, which is signaled as it ends, then on what the job lists next (a child one of them
/// started meanwhile), until it lists none. A job's completion port would say when its last
/// process ended, but a job takes one port, and a job a driver handed carries the driver's, in
/// the driver's process. `Err` with how many were left when `long` ran out.
fn gone_within(job: &Handle, long: Duration) -> Result<(), usize> {
    /// The most handles one `WaitForMultipleObjects` takes (`MAXIMUM_WAIT_OBJECTS`).
    const AT_ONCE: usize = 64;
    let until = Instant::now() + long;
    loop {
        let Some((list, count)) = listed(job) else { return Ok(()) };
        if count == 0 {
            return Ok(());
        }
        let mut held: [Option<Handle>; AT_ONCE] = std::array::from_fn(|_| None);
        let mut waited_on = 0;
        let ids = std::iter::once(list.head.ProcessIdList[0]).chain(list.more.iter().copied()).take(count);
        let mut ids = ids.peekable();
        while ids.peek().is_some() {
            let mut open = 0;
            for id in ids.by_ref() {
                let Ok(id) = u32::try_from(id) else { continue };
                // SAFETY: takes a number and flags, reads no memory of this process; the handle
                // it gives, or null for a process already gone, is owned by the `Handle`.
                let process = Handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, id) });
                if process.0.is_null() {
                    continue;
                }
                let mut inside = 0;
                // SAFETY: both handles are open and owned for the call; `inside` is a live
                // BOOL for it to write. A number another process took since the list was read
                // is not in the job, and is not waited for.
                if unsafe { IsProcessInJob(process.0, job.0, &mut inside) } == 0 || inside == 0 {
                    continue;
                }
                held[open] = Some(process);
                open += 1;
                if open == AT_ONCE {
                    break;
                }
            }
            if open == 0 {
                continue;
            }
            waited_on += open;
            let handles: [HANDLE; AT_ONCE] = std::array::from_fn(|at| held[at].as_ref().map_or(null_mut(), |it| it.0));
            let left = until.saturating_duration_since(Instant::now());
            let ms = u32::try_from(left.as_millis()).unwrap_or(INFINITE - 1);
            // SAFETY: the first `open` handles are open process handles `held` owns until the
            // wait returns; the call reads them and nothing more.
            let waited = unsafe { WaitForMultipleObjects(open as u32, handles.as_ptr(), 1, ms) };
            for it in &mut held {
                *it = None;
            }
            if waited == WAIT_TIMEOUT || waited == WAIT_FAILED {
                return Err(count);
            }
        }
        if waited_on == 0 {
            // Every one listed was gone by its open, and the job lets go of it as it ends.
            if Instant::now() >= until {
                return Err(count);
            }
            std::thread::yield_now();
        }
    }
}

/// Copies of the launcher's standard input, output and error the program inherits, `None` for
/// one the launcher has not.
fn standard() -> Result<[Option<Handle>; 3], String> {
    let mut out = [None, None, None];
    let which: [(STD_HANDLE, &str); 3] =
        [(STD_INPUT_HANDLE, "input"), (STD_OUTPUT_HANDLE, "output"), (STD_ERROR_HANDLE, "error")];
    for (at, (which, name)) in which.into_iter().enumerate() {
        // SAFETY: `which` is one of the three STD_HANDLE values; the call reads no memory of
        // this process, and the handle it gives is the process's, not this function's to close.
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut copy: HANDLE = null_mut();
        // SAFETY: the call takes nothing and reads no memory; the pseudo-handle it gives is a
        // constant that is never closed, so nothing here closes it.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: `handle` is this process's standard handle, just checked neither null nor
        // INVALID_HANDLE_VALUE, and `process` names this process on both sides; `copy` is a
        // live HANDLE for the call to set, owned from then on by the `Handle` below, which
        // closes it once.
        let copied = unsafe { DuplicateHandle(process, handle, process, &mut copy, 0, 1, DUPLICATE_SAME_ACCESS) };
        if copied == 0 {
            // alloc: the error, once, and the sandbox stops.
            return Err(failed(format!("passing the program the launcher's standard {name}"), "DuplicateHandle"));
        }
        out[at] = Some(Handle(copy));
    }
    Ok(out)
}

/// The attribute list of the program's process.
struct Attributes(Vec<usize>);

impl Attributes {
    fn new(count: u32) -> Result<Attributes, String> {
        let mut size = 0;
        // SAFETY: a null list asks for the size alone and is never written; `size` is a live
        // `usize` for the call to set.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(failed("sizing the program's attribute list", "InitializeProcThreadAttributeList"));
        }
        // alloc: once per sandbox, the attribute list of the size Windows asked for.
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buffer` is at least `size` bytes, the size the call above asked for the same
        // `count`, live and exclusive for it, and aligned to a pointer's alignment, which the
        // list's fields need; a smaller buffer would be written past.
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size) } == 0 {
            return Err(failed("making the program's attribute list", "InitializeProcThreadAttributeList"));
        }
        Ok(Attributes(buffer))
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// Sets `attribute` to the `size` bytes at `value`, which the list points to.
    ///
    /// # Safety
    ///
    /// `value` is readable for `size` bytes, holds what `attribute` names, and stays there,
    /// unchanged, until the list is dropped: the list keeps the pointer, and CreateProcessW
    /// reads through it.
    unsafe fn set(&mut self, attribute: u32, value: *const c_void, size: usize, what: &str) -> Result<(), String> {
        // SAFETY: `self.0` was made a list for this many attributes by `new`, and is deleted
        // only when `self` drops; `value` and `size` are as the caller promised (`# Safety`).
        let set = unsafe {
            UpdateProcThreadAttribute(self.list(), 0, attribute as usize, value, size, null_mut(), null())
        };
        match set {
            0 => Err(failed(what, "UpdateProcThreadAttribute")),
            _ => Ok(()),
        }
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: an `Attributes` exists only once InitializeProcThreadAttributeList made its
        // list (`new`), and this drop deletes it once; the buffer is freed after, as the Vec
        // drops.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// The session's [`LOCK`], held while it is not dropped.
struct Lock {
    mutex: HANDLE,
    /// Not `Send`: the mutex is released by the thread that took it.
    _thread: PhantomData<*const ()>,
}

/// The handle of the session's [`LOCK`], opened by the first [`Lock::take`] of the process and
/// never closed, so a run that takes the lock twice opens it once.
static MUTEX: OnceLock<usize> = OnceLock::new();

impl Lock {
    fn take() -> Result<Lock, String> {
        let mutex = match MUTEX.get() {
            Some(&kept) => kept as HANDLE,
            None => {
                let name = literal(LOCK);
                // SAFETY: `name` ends in NUL (`literal`) and lives in this frame for the call,
                // which only reads it; null security is the default.
                let opened = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
                if opened.is_null() {
                    // alloc: the error, once, and the sandbox stops.
                    return Err(failed(format!("opening the lock on the sandbox's grants {LOCK}"), "CreateMutexW"));
                }
                let kept = *MUTEX.get_or_init(|| opened as usize);
                if kept != opened as usize {
                    // SAFETY: another thread kept its own handle first, so `opened` is this
                    // call's alone, closed once here and never used again.
                    unsafe { CloseHandle(opened) };
                }
                kept as HANDLE
            }
        };
        // SAFETY: `mutex` is the handle in MUTEX, which is opened once and never closed, so it
        // names the session's mutex for as long as the process lives.
        match unsafe { WaitForSingleObject(mutex, INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Lock { mutex, _thread: PhantomData }),
            // alloc: the error, once, and the sandbox stops.
            _ => Err(failed(format!("taking the lock on the sandbox's grants {LOCK}"), "WaitForSingleObject")),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: `take` returns a `Lock` only once this thread's wait took the mutex, and a
        // `Lock` stays on that thread (`_thread` makes it not `Send`); the handle in MUTEX is
        // never closed. Releasing from another thread fails and leaves the lock held for the
        // session.
        unsafe { ReleaseMutex(self.mutex) };
    }
}

/// The DACL of a file or directory, and the security descriptor it is in.
struct Dacl {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl Dacl {
    fn of(name: &[u16], path: &Path, what: &str) -> Result<Dacl, String> {
        assert_eq!(name.last(), Some(&0), "a path's name for GetNamedSecurityInfoW ends in NUL");
        let mut it = Dacl { descriptor: null_mut(), dacl: null_mut() };
        // SAFETY: `name` ends in NUL (asserted above; every caller has it from `wide`), so the
        // call reads no further than the slice, and only reads it. The DACL and descriptor
        // out-pointers are live fields of `it`, the others null as allowed; the DACL points into
        // the descriptor, which `it`'s drop frees once.
        let code = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut it.dacl,
                null_mut(),
                &mut it.descriptor,
            )
        };
        match code {
            ERROR_SUCCESS => Ok(it),
            // alloc: the error, once, and the sandbox stops.
            _ => Err(failed_with(format!("{what} on {}", path.display()), "GetNamedSecurityInfoW", code as i32)),
        }
    }

    /// The DACL of what `file` has open, which it opened with READ_CONTROL.
    fn of_handle(file: &Handle, path: &Path, what: &str) -> Result<Dacl, String> {
        let mut it = Dacl { descriptor: null_mut(), dacl: null_mut() };
        // SAFETY: `file` owns an open handle with READ_CONTROL (`open`). The DACL and
        // descriptor out-pointers are live fields of `it`, the others null as allowed; the DACL
        // points into the descriptor, which `it`'s drop frees once.
        let code = unsafe {
            GetSecurityInfo(
                file.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut it.dacl,
                null_mut(),
                &mut it.descriptor,
            )
        };
        match code {
            ERROR_SUCCESS => Ok(it),
            // alloc: the error, once, and the sandbox stops.
            _ => Err(failed_with(format!("{what} on {}", path.display()), "GetSecurityInfo", code as i32)),
        }
    }

    /// Whether one of `sids` has `mask` on what this is set on and, for a directory, on what is
    /// below it, by the entries that allow it.
    /// A null DACL lets everyone in, so it covers any.
    fn covers(&self, sids: &[PSID], mask: u32, dir: bool) -> bool {
        if self.dacl.is_null() {
            return true;
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: `self.dacl` is not null (checked above), so it is the DACL GetNamedSecurityInfoW
        // gave, inside the descriptor `self` frees only when dropped. AclSizeInformation names an
        // ACL_SIZE_INFORMATION, and `info` is a live, exclusive one of the size the call is told.
        let asked = unsafe {
            GetAclInformation(
                self.dacl,
                (&raw mut info).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if asked == 0 {
            return false;
        }
        let (mut here, mut dirs, mut files) = (0, 0, 0);
        for at in 0..info.AceCount {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `self.dacl` is a live DACL (above) and `at` is below the entry count it
            // reported; `ace` is a live pointer for the call to set to the entry, inside the DACL.
            if unsafe { GetAce(self.dacl, at, &mut ace) } == 0 {
                continue;
            }
            // SAFETY: GetAce succeeded, so `ace` points to an entry of the DACL, which `self`
            // keeps alive; every entry starts with its header, at the 4-byte alignment entries
            // have in an ACL, and the header is `Copy`, so this reads it and holds no reference.
            let header = unsafe { *ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: the header's type, checked just above, says the entry is an
            // ACCESS_ALLOWED_ACE, whose size Windows checked when it made the DACL, so `Mask`
            // is inside the entry, initialized and 4-byte aligned as the entry is.
            let mask_of = unsafe { (*allowed).Mask };
            // SAFETY: as above, the entry is an ACCESS_ALLOWED_ACE; this only takes the address
            // of `SidStart`, where its SID begins, and reads nothing.
            let entry_sid = unsafe { (&raw mut (*allowed).SidStart).cast::<c_void>() };
            // SAFETY: `entry_sid` is the SID of an entry of the live DACL, and every one of
            // `sids` is the container's or ALL APPLICATION PACKAGES' in the static [`Kept`], or
            // the run's capability, live while its `Made` is (`run`, `covered`, `grant`); the
            // call only reads both.
            if !sids.iter().any(|it| unsafe { EqualSid(*it, entry_sid) } != 0) {
                continue;
            }
            let given = mapped(mask_of);
            let flags = u32::from(header.AceFlags);
            if flags & INHERIT_ONLY_ACE == 0 {
                here |= given;
            }
            if flags & NO_PROPAGATE_INHERIT_ACE == 0 {
                if flags & CONTAINER_INHERIT_ACE != 0 {
                    dirs |= given;
                }
                if flags & OBJECT_INHERIT_ACE != 0 {
                    files |= given;
                }
            }
        }
        let has = |it: u32| it & mask == mask;
        has(here) && (!dir || (has(dirs) && has(files)))
    }
}

impl Drop for Dacl {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: `descriptor` is not null, so GetNamedSecurityInfoW made it with LocalAlloc
            // (`of`), and says to free it with LocalFree; `Dacl` is never copied, so this frees
            // it once, and `dacl`, which points into it, is not read after.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

/// `mask` with its generic rights as the specific ones of a file.
fn mapped(mask: u32) -> u32 {
    let mut out = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    for (generic, specific) in [
        (GENERIC_READ, FILE_GENERIC_READ),
        (GENERIC_WRITE, FILE_GENERIC_WRITE),
        (GENERIC_EXECUTE, FILE_GENERIC_EXECUTE),
        (GENERIC_ALL, FILE_ALL_ACCESS),
    ] {
        if mask & generic != 0 {
            out |= specific;
        }
    }
    out
}

fn trustee(sid: PSID) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
        ptstrName: sid.cast(),
    }
}

/// What [`covered`] saw of a path that is there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Seen {
    dir: bool,
    covered: bool,
}

/// Whether `path` is there (following links) and, if so, whether it is a directory and one of
/// `already` has `mask` on it (and below it, for a directory) or its DACL is null and lets
/// everyone in. One open, one attribute read and one DACL read, all on one handle, where a stat
/// by name and then a DACL read by name would resolve the path twice.
fn covered(path: &Path, already: &[PSID], mask: u32, what: &str) -> Result<Option<Seen>, String> {
    match open(path, what)? {
        Some(file) => inspect(&file, path, already, mask, what).map(Some),
        None => Ok(None),
    }
}

/// `path` opened to read its attributes and DACL, following links; `None` when it is not there.
fn open(path: &Path, what: &str) -> Result<Option<Handle>, String> {
    let name = wide(path.as_os_str(), "a path the sandbox grants")?;
    // SAFETY: `name` ends in NUL (`wide`) and lives in this frame, and the call only reads it;
    // null security attributes and template are allowed. The handle it gives, or
    // INVALID_HANDLE_VALUE, is owned by the `Handle`, whose drop closes only a valid one.
    let file = Handle(unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    });
    if file.0 == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => Ok(None),
            // alloc: the error, once, and the sandbox stops.
            _ => Err(format!("{what} on {} (CreateFileW): {error}", path.display())),
        };
    }
    Ok(Some(file))
}

/// Whether what `file` has open (by [`open`]) is a directory, and whether one of `already` has
/// `mask` on it as [`covered`] says.
fn inspect(file: &Handle, path: &Path, already: &[PSID], mask: u32, what: &str) -> Result<Seen, String> {
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: `file` owns an open handle (`open` returns only a valid one), opened with
    // FILE_READ_ATTRIBUTES; FileBasicInfo names a FILE_BASIC_INFO, and `basic` is a live,
    // exclusive one of the size the call is told, which it writes and nothing more.
    let asked = unsafe {
        GetFileInformationByHandleEx(
            file.0,
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if asked == 0 {
        // alloc: the error, once, and the sandbox stops.
        return Err(failed(format!("{what} on {}", path.display()), "GetFileInformationByHandleEx"));
    }
    let dir = basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let current = Dacl::of_handle(file, path, what)?;
    Ok(Seen { dir, covered: current.covers(already, mask, dir) })
}

/// Whether `path` is a directory, following a link as `Path::is_dir` does, with one attribute
/// read for what is not a link.
fn directory(path: &Path) -> bool {
    let Ok(name) = wide(path.as_os_str(), "a directory") else { return false };
    // SAFETY: `name` ends in NUL (`wide`) and lives in this frame; the call only reads it.
    let attributes = unsafe { GetFileAttributesW(name.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return path.is_dir();
    }
    attributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// A security descriptor whose DACL gives one SID `mask` on a directory and everything below it,
/// for making directories with: the system merges in what the new directory inherits from its
/// parent, as the descriptor is not protected, so the directory gets the grant with no DACL read
/// or rewrite and nothing below it to propagate to.
struct Inheritable {
    /// The DACL, `u32`s for an ACL's and its entries' 4-byte alignment.
    _acl: Vec<u32>,
    /// Points at `acl`'s heap buffer, which moving `Inheritable` leaves where it is.
    descriptor: Box<SECURITY_DESCRIPTOR>,
}

impl Inheritable {
    fn new(sid: PSID, mask: u32) -> Result<Inheritable, String> {
        // SAFETY: `sid` is a SID of the run's `Made` (`run`), valid while it is; the call only
        // reads it.
        let sid_size = unsafe { GetLengthSid(sid) } as usize;
        let size = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_size;
        // alloc: once per directory the sandbox makes, the ACL of its one grant.
        let mut acl = vec![0u32; size.div_ceil(size_of::<u32>())];
        let bytes = (acl.len() * size_of::<u32>()) as u32;
        // SAFETY: `acl` is `bytes` bytes, live, exclusive and 4-byte aligned, which the call is
        // told and writes no further than.
        if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), bytes, ACL_REVISION) } == 0 {
            return Err(failed("making the sandbox's write grant", "InitializeAcl"));
        }
        // SAFETY: `acl` is the ACL just made, sized above for one ACCESS_ALLOWED_ACE with this
        // SID, so the entry fits; `sid` is valid (above) and only read.
        let added = unsafe {
            AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                mask,
                sid,
            )
        };
        if added == 0 {
            return Err(failed("making the sandbox's write grant", "AddAccessAllowedAceEx"));
        }
        // alloc: as `acl` is, at an address that stays while the attributes point to it.
        let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
        // SAFETY: `descriptor` is a live, exclusive SECURITY_DESCRIPTOR, the absolute form the
        // call makes at this revision.
        if unsafe { InitializeSecurityDescriptor((&raw mut *descriptor).cast(), SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(failed("making the sandbox's write grant", "InitializeSecurityDescriptor"));
        }
        // SAFETY: `descriptor` was just initialized; the DACL it is given is `acl`'s heap buffer,
        // which `Inheritable` keeps beside it, unchanged, for as long as the descriptor is used.
        if unsafe { SetSecurityDescriptorDacl((&raw mut *descriptor).cast(), 1, acl.as_ptr().cast(), 0) } == 0 {
            return Err(failed("making the sandbox's write grant", "SetSecurityDescriptorDacl"));
        }
        Ok(Inheritable { _acl: acl, descriptor })
    }

    /// Makes `dir` with the grant in it; an error, and nothing made, when it is there already or
    /// its parent is not.
    fn make_dir(&self, dir: &Path) -> std::io::Result<()> {
        let name = wide(dir.as_os_str(), "a directory the sandbox writes")
            .map_err(|why| std::io::Error::new(ErrorKind::InvalidInput, why))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&raw const *self.descriptor).cast_mut().cast(),
            bInheritHandle: 0,
        };
        // SAFETY: `name` ends in NUL (`wide`) and lives in this frame; `attributes` points at
        // `self.descriptor`, a valid absolute descriptor whose DACL is `self._acl` (`new`), both
        // live while `self` is, and the call only reads them.
        match unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } {
            0 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    /// Makes `dir`, and its missing parents as `create_dir_all` does, with the grant in `dir`:
    /// `true` when it made it, `false` when a directory was there.
    fn make_dir_all(&self, dir: &Path) -> std::io::Result<bool> {
        let mut error = match self.make_dir(dir) {
            Ok(()) => return Ok(true),
            Err(error) => error,
        };
        if error.kind() == ErrorKind::NotFound
            && let Some(parent) = dir.parent()
        {
            std::fs::create_dir_all(parent)?;
            error = match self.make_dir(dir) {
                Ok(()) => return Ok(true),
                Err(error) => error,
            };
        }
        match error.kind() == ErrorKind::AlreadyExists && directory(dir) {
            true => Ok(false),
            false => Err(error),
        }
    }
}

/// Gives `sid` `mask` on `path`, inheritably when it is a directory `dir`, unless one of
/// `already` has it there: whether an entry was added.
fn grant(path: &Path, dir: bool, sid: PSID, already: &[PSID], mask: u32, what: &str) -> Result<bool, String> {
    let name = wide(path.as_os_str(), "a path the sandbox grants")?;
    let current = Dacl::of(&name, path, what)?;
    // A null DACL lets everyone do everything, and has no entries to add to.
    if current.dacl.is_null() || current.covers(already, mask, dir) {
        return Ok(false);
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if dir { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE },
        Trustee: trustee(sid),
    };
    set_dacl(&name, path, &current, &access, what)?;
    Ok(true)
}

/// Takes every entry for `sid` out of the DACL of `path`.
fn revoke(path: &Path, sid: PSID, what: &str) -> Result<(), String> {
    let name = wide(path.as_os_str(), "a path the sandbox granted")?;
    let current = Dacl::of(&name, path, what)?;
    if current.dacl.is_null() {
        return Ok(());
    }
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0,
        grfAccessMode: REVOKE_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: trustee(sid),
    };
    set_dacl(&name, path, &current, &access, what)
}

fn set_dacl(name: &[u16], path: &Path, current: &Dacl, access: &EXPLICIT_ACCESS_W, what: &str) -> Result<(), String> {
    assert_eq!(name.last(), Some(&0), "a path's name for SetNamedSecurityInfoW ends in NUL");
    let mut new: *mut ACL = null_mut();
    // SAFETY: `access` is one live EXPLICIT_ACCESS_W, the count the call is told, whose trustee
    // is a SID of the run's `Made` (`trustee`); `current.dacl` is the DACL GetNamedSecurityInfoW
    // gave, live while `current` is, and only read. `new` is a live pointer for the call to set.
    let code = unsafe { SetEntriesInAclW(1, access, current.dacl, &mut new) };
    if code != ERROR_SUCCESS {
        // alloc: the error, once, and the sandbox stops.
        return Err(failed_with(format!("{what} on {}", path.display()), "SetEntriesInAclW", code as i32));
    }
    // SAFETY: `name` ends in NUL (asserted above; `wide` made it) and is only read, and `new` is
    // the DACL SetEntriesInAclW just made, freed only below; owner, group and SACL are null as
    // DACL_SECURITY_INFORMATION alone allows.
    let code = unsafe {
        SetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), new, null())
    };
    // SAFETY: SetEntriesInAclW made `new` with LocalAlloc and says to free it with LocalFree;
    // SetNamedSecurityInfoW copied it, so nothing reads it after, and it is freed once.
    unsafe { LocalFree(new.cast()) };
    match code {
        ERROR_SUCCESS => Ok(()),
        // alloc: the error, once, and the sandbox stops.
        _ => Err(failed_with(format!("{what} on {}", path.display()), "SetNamedSecurityInfoW", code as i32)),
    }
}

/// What a run takes back as it ends, however it ends: the write grants it added to directories
/// that stay, and its temporary directory, grant and all.
struct Granted {
    sid: PSID,
    writes: Vec<PathBuf>,
    temp: PathBuf,
}

impl Drop for Granted {
    fn drop(&mut self) {
        if !self.writes.is_empty() {
            let lock = Lock::take();
            if let Err(why) = &lock {
                eprintln!("blaze: the sandbox's write grants are taken back without the lock: {why}");
            }
            for dir in &self.writes {
                if let Err(why) = revoke(dir, self.sid, "taking back the sandbox's write") {
                    eprintln!("blaze: {why}");
                }
            }
            drop(lock);
        }
        if let Err(error) = std::fs::remove_dir_all(&self.temp) {
            eprintln!("blaze: removing the run's temporary directory {}: {error}", self.temp.display());
        }
    }
}

/// The jobs of the runs in progress, for the console's interrupt to end.
// alloc: none: an empty `Vec` in a static, which grows once per sandbox.
static JOBS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// How many runs have not taken back their grants yet, for the console's close to wait on.
static RUNS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// Counts a run in [`RUNS`] while it is not dropped.
struct Running;

impl Running {
    fn start() -> Running {
        *RUNS.0.lock() += 1;
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        *RUNS.0.lock() -= 1;
        RUNS.1.notify_all();
    }
}

/// Names a job in [`JOBS`] while it is not dropped.
struct Registered(usize);

impl Registered {
    fn new(job: &Handle) -> Registered {
        let job = job.0 as usize;
        JOBS.lock().push(job);
        Registered(job)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut jobs = JOBS.lock();
        if let Some(at) = jobs.iter().position(|it| *it == self.0) {
            jobs.swap_remove(at);
        }
    }
}

/// Ctrl-C, Ctrl-Break and closing the console end the program's job rather than the launcher,
/// which then takes its grants back and ends as the program did. On a close Windows ends the
/// launcher once this returns, so it waits for that first.
///
/// # Safety
///
/// Called only by Windows, as the console's handler `on_interrupt` set; it reads nothing
/// through its argument, so any `event` is sound.
unsafe extern "system" fn interrupted(event: u32) -> BOOL {
    if !matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
        return 0;
    }
    let wait = match event {
        CTRL_CLOSE_EVENT => Duration::from_secs(4),
        _ => Duration::ZERO,
    };
    end_every_run(STATUS_CONTROL_C_EXIT as u32, wait);
    1
}

/// Ends the job of every run in progress with `code`, then waits up to `wait` for the runs to
/// take their grants back: what a console's close does, and a warm launcher whose driver is
/// gone ([`serve`]).
pub fn end_every_run(code: u32, wait: Duration) {
    let jobs = JOBS.lock();
    for &job in jobs.iter() {
        // SAFETY: a job is named in JOBS only while its `Registered` lives, which `run` declares
        // after `made` (so drops it first) and which takes the job out under the lock this holds:
        // the handle is open for the call, not one closed whose number another object took.
        unsafe { TerminateJobObject(job as HANDLE, code) };
    }
    drop(jobs);
    if !wait.is_zero() {
        let mut runs = RUNS.0.lock();
        RUNS.1.wait_while_for(&mut runs, |runs| *runs > 0, wait);
    }
}

fn on_interrupt() -> Result<(), String> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            // SAFETY: `interrupted` has the PHANDLER_ROUTINE signature, is a `fn` item that lives
            // as long as the process, and is sound on any thread for any event (its `# Safety`):
            // it only locks JOBS and RUNS, ends jobs and waits (`end_every_run`). Windows calls it
            // on a thread of its own, so it touches nothing that is not `Sync`.
            match unsafe { SetConsoleCtrlHandler(Some(interrupted), 1) } {
                0 => Err(failed("passing the console's interrupt to the sandbox", "SetConsoleCtrlHandler")),
                _ => Ok(()),
            }
        })
        // clone: `Ok(())` copies nothing; an error is copied once, and the sandbox stops.
        .clone()
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, JOB_OBJECT_LIMIT_JOB_MEMORY};

    use super::*;

    #[test]
    fn generic_rights_are_mapped_to_a_files() {
        assert_eq!(mapped(GENERIC_READ | GENERIC_EXECUTE), READ);
        assert_eq!(mapped(GENERIC_ALL), FILE_ALL_ACCESS);
        assert_eq!(mapped(FILE_GENERIC_READ), FILE_GENERIC_READ);
    }

    #[test]
    fn a_program_writes_where_it_is_granted_alone_and_ends_as_it_ended() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let cmd = root.join("System32").join("cmd.exe");
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (written, denied) = (base.join("w"), base.join("d"));
        std::fs::create_dir_all(&denied).unwrap();
        let spec = |args: &[&OsStr]| Spec {
            program: cmd.clone(),
            args: args.iter().map(|it| it.to_os_string()).collect(),
            env: vec![("SystemRoot".into(), root.clone().into_os_string())],
            dir: base.clone(),
            read: vec![base.clone()],
            write: vec![written.clone()],
            network: false,
            trace: false,
            services: Vec::new(),
            live: base.join("live"),
            report: base.join("report"),
            not_inputs: vec![cmd.clone()],
        };
        let (c, echo, hi) = (OsStr::new("/c"), OsStr::new("echo"), OsStr::new("hi>"));

        let inside = written.join("out.txt");
        assert_eq!(run(&spec(&[c, echo, hi, inside.as_os_str()]), Given::own()), Ok(0));
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "hi");

        let outside = denied.join("out.txt");
        assert_ne!(run(&spec(&[c, echo, hi, outside.as_os_str()]), Given::own()), Ok(0));
        assert!(!outside.exists());

        let exit = spec(&[c, OsStr::new("exit"), OsStr::new("7")]);
        assert_eq!(run(&exit, Given::own()), Ok(7));
        assert_eq!(temporary(&exit), temporary(&exit.clone()));
        assert_ne!(temporary(&exit), temporary(&spec(&[c, OsStr::new("exit"), OsStr::new("8")])));
        assert!(!temporary(&exit).unwrap().exists(), "the run's temporary directory was left behind");
        _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_read_seen_granted_without_the_lock_is_one_granting_would_not_add() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-covered-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (dir, file) = (base.join("d"), base.join("f.txt"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "x").unwrap();
        let made = make(false, Given::own()).unwrap();
        let readers = [sid(&made.kept.container), sid(&made.kept.any)];
        for (path, is_dir) in [(&dir, true), (&file, false)] {
            let before = covered(path, &readers, READ, "test").unwrap().unwrap();
            assert_eq!(before.dir, is_dir, "{}", path.display());
            let added = {
                let _lock = Lock::take().unwrap();
                grant(path, is_dir, readers[0], &readers, READ, "test").unwrap()
            };
            assert_eq!(added, !before.covered, "{}", path.display());
            assert_eq!(covered(path, &readers, READ, "test").unwrap(), Some(Seen { dir: is_dir, covered: true }));
            let _lock = Lock::take().unwrap();
            assert!(!grant(path, is_dir, readers[0], &readers, READ, "test").unwrap(), "{}", path.display());
        }
        _ = std::fs::remove_dir_all(&base);
    }

    /// The check `run` made before reading through one handle: a stat by name, then the DACL by
    /// name.
    fn covered_by_name(path: &Path, already: &[PSID], mask: u32) -> Option<Seen> {
        let dir = std::fs::metadata(path).ok()?.is_dir();
        let name = wide(path.as_os_str(), "test").unwrap();
        let current = Dacl::of(&name, path, "test").unwrap();
        Some(Seen { dir, covered: current.dacl.is_null() || current.covers(already, mask, dir) })
    }

    #[test]
    fn a_path_read_through_one_handle_is_seen_as_by_name() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-handle-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (dir, file, missing) = (base.join("d"), base.join("f.txt"), base.join("gone").join("x"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "x").unwrap();
        let made = make(false, Given::own()).unwrap();
        let readers = [sid(&made.kept.container), sid(&made.kept.any)];
        let windows = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        for path in [&dir, &file, &missing, &base, &windows.join("System32"), &windows.join("win.ini")] {
            for (sids, mask) in [(&readers[..], READ), (&readers[..1], READ), (&readers[..], WRITE)] {
                assert_eq!(covered(path, sids, mask, "test").unwrap(), covered_by_name(path, sids, mask), "{}", path.display());
            }
        }
        assert_eq!(covered(&missing, &readers, READ, "test").unwrap(), None);
        _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_directory_made_with_its_grant_also_keeps_what_it_inherits() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-made-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let made = make(false, Given::own()).unwrap();
        let run = made.run.sid();
        let writable = Inheritable::new(run, WRITE).unwrap();

        let fresh = base.join("fresh");
        writable.make_dir(&fresh).unwrap();
        assert_eq!(covered(&fresh, &[run], WRITE, "test").unwrap(), Some(Seen { dir: true, covered: true }));
        // Merged with what it inherits: the user who made it still writes, lists and removes in it.
        let inside = fresh.join("a.txt");
        std::fs::write(&inside, "x").unwrap();
        assert_eq!(covered(&inside, &[run], WRITE, "test").unwrap(), Some(Seen { dir: false, covered: true }));
        assert_eq!(std::fs::read_dir(&fresh).unwrap().count(), 1);
        assert_eq!(writable.make_dir(&fresh).unwrap_err().kind(), ErrorKind::AlreadyExists);

        assert!(!writable.make_dir_all(&fresh).unwrap(), "a directory there is found, not made");
        let nested = base.join("p").join("q");
        assert!(writable.make_dir_all(&nested).unwrap());
        assert!(covered(&nested, &[run], WRITE, "test").unwrap().unwrap().covered);
        assert!(writable.make_dir_all(&inside).is_err(), "a file is not a directory");

        // What `run` takes back from a directory it made, and what a grant by name would add.
        revoke(&nested, run, "test").unwrap();
        assert!(!covered(&nested, &[run], WRITE, "test").unwrap().unwrap().covered);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_directory_is_told_as_by_path() {
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-directory-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let (dir, file) = (base.join("d"), base.join("f.txt"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "x").unwrap();
        for path in [&base, &dir, &file, &base.join("gone"), &file.join("below")] {
            assert_eq!(directory(path), path.is_dir(), "{}", path.display());
        }
        _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_empty_job_is_ended_at_once() {
        let job = job().unwrap();
        let started = Instant::now();
        end(&job, 1);
        assert!(started.elapsed() < ENDING);
    }

    /// A process that runs for a minute unless it is ended, put in `job`.
    fn in_job(job: &Handle) -> std::process::Child {
        use std::os::windows::io::AsRawHandle as _;
        let child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // SAFETY: both handles are open and owned for the call, which reads no memory.
        assert_ne!(unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) }, 0);
        child
    }

    #[test]
    fn an_end_waits_for_its_processes_to_be_gone_not_for_its_limit() {
        let job = job().unwrap();
        let mut child = in_job(&job);
        let started = Instant::now();
        end(&job, 7);
        assert!(started.elapsed() < ENDING / 10, "ended after {:?}", started.elapsed());
        assert_eq!(child.try_wait().unwrap().and_then(|it| it.code()), Some(7));
        assert_eq!(listed(&job).unwrap().1, 0);
    }

    #[test]
    fn an_end_waits_for_a_child_that_outlived_the_process_that_started_it() {
        use std::os::windows::io::AsRawHandle as _;
        let job = job().unwrap();
        // A second in, once it is in the job, it starts a ping that runs on after it ends.
        let mut parent = std::process::Command::new("cmd")
            .args(["/c", "ping -n 2 127.0.0.1 >nul & start /b ping -n 60 127.0.0.1 >nul"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // SAFETY: both handles are open and owned for the call, which reads no memory.
        assert_ne!(unsafe { AssignProcessToJobObject(job.0, parent.as_raw_handle()) }, 0);
        parent.wait().unwrap();
        assert_eq!(listed(&job).unwrap().1, 1, "the child runs on in the job");
        let started = Instant::now();
        end(&job, 7);
        assert!(started.elapsed() < ENDING / 10, "ended after {:?}", started.elapsed());
        assert_eq!(listed(&job).unwrap().1, 0);
    }

    #[test]
    fn a_wait_for_a_process_that_runs_on_gives_up_at_its_limit() {
        let job = job().unwrap();
        let mut child = in_job(&job);
        let started = Instant::now();
        assert_eq!(gone_within(&job, Duration::from_millis(50)), Err(1));
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(child.try_wait().unwrap().is_none());
        end(&job, 1);
    }

    /// A job as a driver makes one for a spawn: `flags`, and a memory limit.
    fn drivers_job(flags: u32) -> Handle {
        // SAFETY: as in `job`.
        let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
        assert!(!job.0.is_null());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = flags | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = 1 << 30;
        // SAFETY: JobObjectExtendedLimitInformation names a JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
        unsafe { set_job(&job, JobObjectExtendedLimitInformation, &limits, "test") }.unwrap();
        job
    }

    fn limits_of(job: &Handle) -> JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: as in `driven`.
        let asked = unsafe {
            QueryInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        };
        assert_ne!(asked, 0);
        limits
    }

    /// A copy of `handle` in this process, as a driver duplicates one into the launcher.
    fn duplicated(handle: HANDLE, inherit: bool) -> u64 {
        let mut copy: HANDLE = null_mut();
        // SAFETY: as in `standard`: `handle` is open, and `copy` is the test's from here on.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: as in `standard`.
        let copied = unsafe {
            DuplicateHandle(process, handle, process, &mut copy, 0, BOOL::from(inherit), DUPLICATE_SAME_ACCESS)
        };
        assert_ne!(copied, 0);
        copy as usize as u64
    }

    #[test]
    fn a_job_the_driver_hands_is_checked_and_keeps_its_memory_limit() {
        let job = driven(drivers_job(HANDED_LIMITS)).unwrap();
        let limits = limits_of(&job);
        assert_eq!(limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_MEMORY);
        assert_eq!(limits.JobMemoryLimit, 1 << 30);
        let why = driven(drivers_job(JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION)).err().unwrap();
        assert!(why.contains("killing what is left"), "{why}");
        // SAFETY: a frame that names no handle adopts none.
        let none = unsafe { Given::adopt(&blaze_core::Handed { run: 1, stdin: 0, stdout: 0, stderr: 0, job: 0 }) };
        assert!(none.driver && none.job.is_none() && none.standard.iter().all(Option::is_none));
        if probe().is_ok() {
            assert!(make(false, none).err().unwrap().contains("no job"));
        }
    }

    #[test]
    fn a_run_handed_the_drivers_handles_writes_to_them_in_the_drivers_job() {
        if let Err(why) = probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let cmd = root.join("System32").join("cmd.exe");
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-handed-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let out = base.join("stdout.txt");
        let file = std::fs::File::create(&out).unwrap();
        let ours = drivers_job(HANDED_LIMITS);
        let handed = blaze_core::Handed {
            run: 1,
            stdin: 0,
            stdout: duplicated(std::os::windows::io::AsRawHandle::as_raw_handle(&file), true),
            stderr: 0,
            job: duplicated(ours.0, false),
        };
        let spec = Spec {
            program: cmd.clone(),
            args: vec!["/c".into(), "echo handed& exit 3".into()],
            env: vec![("SystemRoot".into(), root.clone().into_os_string())],
            dir: base.clone(),
            read: vec![base.clone()],
            write: Vec::new(),
            network: false,
            trace: false,
            services: Vec::new(),
            live: base.join("live"),
            report: base.join("report"),
            not_inputs: vec![cmd],
        };
        // SAFETY: both values were just duplicated for this run, and nothing else names them.
        assert_eq!(run(&spec, unsafe { Given::adopt(&handed) }), Ok(3));
        drop(file);
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "handed");
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: as in `end`.
        let asked = unsafe {
            QueryInformationJobObject(
                ours.0,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        };
        assert_ne!(asked, 0);
        assert!(info.TotalProcesses >= 1, "the program ran in the driver's job");
        assert_eq!(info.ActiveProcesses, 0);
        _ = std::fs::remove_dir_all(&base);
    }
}
