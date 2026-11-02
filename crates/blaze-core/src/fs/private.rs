//! [`crate::Effect::Private`]: a directory made the user's alone, and checked to be with
//! everything under it.
//!
//! On Unix that is each file's owner and mode. The directory is made 0o700 and kept so, so what
//! blaze writes in it is reached by no one else whatever the umask gave each file; a file under
//! it is refused for being another user's or anyone's to write, and for its group's to write
//! while the directory lets its group in. On Windows it is the owner and the access list: a
//! directory made under the user's profile inherits one that lets in the user, the system and
//! the administrators, and one that lets anyone else write is refused.

use std::{io, path::Path};

use crate::{Access, FileName};

/// See [`crate::Effect::Private`]. `dir` is the path of `name`.
pub(crate) fn private(dir: &Path, name: &FileName) -> io::Result<Option<(FileName, Access)>> {
    os::make(dir)?;
    let (access, gate) = os::top(dir)?;
    if access != Access::Alone {
        // clone: the refusal names the directory
        return Ok(Some((name.clone(), access)));
    }
    os::close(dir)?;
    let start = name.as_str().len() + 1;
    for (file, _) in super::list(dir, name)? {
        // alloc: once per file of the store, at its open
        let path = dir.join(&file.as_str()[start..]);
        match os::under(&path, &gate) {
            Ok(Access::Alone) => {}
            Ok(access) => return Ok(Some((file, access))),
            // Removed since it was listed: there is nothing of it to trust.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            // alloc: an error path
            Err(error) => return Err(io::Error::new(error.kind(), format!("{}: {error}", path.display()))),
        }
    }
    Ok(None)
}

#[cfg(unix)]
mod os {
    use std::{
        io,
        os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
        path::Path,
    };

    use crate::Access;

    /// What the bits of a file under the directory must not have, and whose it must be.
    pub(super) struct Gate {
        user: libc::uid_t,
        refused: u32,
    }

    pub(super) fn make(dir: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }

    /// The directory's access, and what of a file's bits under it lets someone else write it:
    /// others' always, the group's while the directory lets its group through.
    pub(super) fn top(dir: &Path) -> io::Result<(Access, Gate)> {
        let meta = std::fs::metadata(dir)?;
        let user = crate::raw::euid();
        let refused = if meta.mode() & 0o010 != 0 { 0o022 } else { 0o002 };
        Ok((of(&meta, user, 0o022), Gate { user, refused }))
    }

    /// Takes the group's and others' bits off the directory.
    pub(super) fn close(dir: &Path) -> io::Result<()> {
        let mut permissions = std::fs::metadata(dir)?.permissions();
        if permissions.mode() & 0o077 != 0 {
            permissions.set_mode(permissions.mode() & !0o077);
            std::fs::set_permissions(dir, permissions)?;
        }
        Ok(())
    }

    pub(super) fn under(path: &Path, gate: &Gate) -> io::Result<Access> {
        Ok(of(&std::fs::metadata(path)?, gate.user, gate.refused))
    }

    fn of(meta: &std::fs::Metadata, user: libc::uid_t, refused: u32) -> Access {
        if meta.uid() != user {
            Access::OtherUser
        } else if meta.mode() & refused != 0 {
            Access::OthersWrite
        } else {
            Access::Alone
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code, reason = "GetSecurityInfo, the token's user and the access list's entries")]
mod os {
    use std::{
        ffi::c_void,
        fs::OpenOptions,
        io,
        os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _},
        path::Path,
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, IsWellKnownSid,
            OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinLocalSystemSid,
        },
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    use crate::Access;

    /// The kinds of entry that let someone in and that keep someone out. Any other kind lets in
    /// on a condition this does not evaluate, and is taken to let in.
    const ALLOWED: u8 = 0;
    const DENIED: u8 = 1;
    /// An entry with this is for what is made in a directory and not for the directory.
    const INHERIT_ONLY: u8 = 0x8;

    /// What changes a file or what is in a directory, or who may: writing and appending (in a
    /// directory, adding a file and adding a directory), deleting it or what is in it, changing
    /// its list or its owner, and the generic rights that stand for those.
    const WRITES: u32 =
        0x2 | 0x4 | 0x40 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x4000_0000 | 0x1000_0000;

    /// An entry that lets in or keeps out, as it is laid out: its identifier starts where
    /// `sid_start` is and is as long as it says itself.
    #[repr(C)]
    struct Entry {
        kind: u8,
        flags: u8,
        size: u16,
        mask: u32,
        sid_start: u32,
    }

    /// The identifier of the user the process runs as, in the buffer that holds it: a
    /// `TOKEN_USER` at its start, in words so that it is aligned.
    pub(super) struct Gate(Vec<u64>);

    impl Gate {
        fn user(&self) -> PSID {
            // SAFETY: the buffer holds what `GetTokenInformation` wrote, which starts with a
            // `TOKEN_USER`; its `Sid` points into the buffer, which lives as long as `self`.
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    /// A handle that is closed when this is dropped.
    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by `OpenProcessToken` and is closed once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// What `GetSecurityInfo` allocated, freed when this is dropped.
    struct Descriptor(*mut c_void);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the pointer came from `GetSecurityInfo`, which says to free it so; once.
            unsafe { LocalFree(self.0) };
        }
    }

    pub(super) fn make(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    pub(super) fn top(dir: &Path) -> io::Result<(Access, Gate)> {
        let gate = Gate(user()?);
        Ok((of(dir, &gate)?, gate))
    }

    /// Nothing: a directory's list is what it inherited, and one that lets others in is refused.
    pub(super) fn close(_: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn under(path: &Path, gate: &Gate) -> io::Result<Access> {
        of(path, gate)
    }

    fn user() -> io::Result<Vec<u64>> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess` takes nothing and returns the process's pseudo-handle,
        // which needs no closing; `token` is where the token's handle goes.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Token(token);
        let mut len = 0;
        // SAFETY: with no buffer the call only says how long one has to be, in `len`.
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
        // alloc: once per check, the token's user
        let mut buffer = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let room = (buffer.len() * 8) as u32;
        // SAFETY: `buffer` is `room` bytes that are this function's to write.
        let got = unsafe { GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), room, &mut len) };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buffer)
    }

    /// Whether `sid` is the user, the system or the administrators.
    ///
    /// # Safety
    ///
    /// `sid` and `user` point at valid identifiers.
    unsafe fn trusted(sid: PSID, user: PSID) -> bool {
        // SAFETY: both are valid identifiers, by the caller.
        unsafe {
            EqualSid(sid, user) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
        }
    }

    fn of(path: &Path, gate: &Gate) -> io::Result<Access> {
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let user = gate.user();
        let (mut owner, mut list): (PSID, *mut ACL) = (ptr::null_mut(), ptr::null_mut());
        let mut descriptor = ptr::null_mut();
        // SAFETY: the handle is of an open file, opened with `READ_CONTROL`; the three pointers
        // are where the owner, the list and the descriptor that holds both are put.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut list,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Ok(Access::Unknown);
        }
        let _descriptor = Descriptor(descriptor);
        // SAFETY: `owner` is in the descriptor, which is alive, and `user` in the gate's buffer.
        if owner.is_null() || !unsafe { trusted(owner, user) } {
            return Ok(Access::OtherUser);
        }
        // No list at all lets everyone do everything.
        if list.is_null() {
            return Ok(Access::OthersWrite);
        }
        // SAFETY: `list` is in the descriptor, which is alive.
        let entries = unsafe { (*list).AceCount };
        for at in 0..u32::from(entries) {
            let mut entry: *mut c_void = ptr::null_mut();
            // SAFETY: `list` is a valid list and `at` is below its count of entries.
            if unsafe { GetAce(list, at, &mut entry) } == 0 {
                return Ok(Access::Unknown);
            }
            // SAFETY: every entry starts with its kind, flags and size, then a mask and an
            // identifier for the kinds read here; `read_unaligned` asks for no alignment.
            let Entry { kind, flags, size, mask, .. } = unsafe { entry.cast::<Entry>().read_unaligned() };
            if kind == DENIED || flags & INHERIT_ONLY != 0 || mask & WRITES == 0 {
                continue;
            }
            if kind != ALLOWED || usize::from(size) < size_of::<Entry>() {
                return Ok(Access::Unknown);
            }
            // SAFETY: the identifier of an entry that lets in starts after its mask, inside the
            // entry, which `size` says holds it.
            let sid: PSID = unsafe { entry.byte_add(std::mem::offset_of!(Entry, sid_start)) };
            // SAFETY: `sid` is in the list, which the descriptor keeps alive, and `user` in the
            // gate's buffer; the owner's own rights are the owner's, who was found trusted.
            let known = unsafe { trusted(sid, user) || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0 };
            if !known {
                return Ok(Access::OthersWrite);
            }
        }
        Ok(Access::Alone)
    }
}

#[cfg(test)]
mod tests;
