# Requests from usercode-sandbox-windows

## 1. windows-sys features for `crates/blaze-cargo/sandbox/windows.rs`

Where: `crates/blaze-cargo/Cargo.toml`, the `windows-sys` line under
`[target.'cfg(windows)'.dependencies]`.

What: add these features (each item's module checked in windows-sys 0.61.2's source):

```
"Win32_Security", "Win32_Security_Isolation", "Win32_Storage_FileSystem", "Win32_System_Console", "Win32_System_JobObjects"
```

so the line reads:

```toml
windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_Networking_WinSock", "Win32_Security", "Win32_Security_Authorization", "Win32_Security_Isolation", "Win32_Storage_FileSystem", "Win32_System_Console", "Win32_System_JobObjects", "Win32_System_SystemServices", "Win32_System_Threading"] }
```

Why, by module:

- `Win32_Security`: `SECURITY_CAPABILITIES`, `SID_AND_ATTRIBUTES`, `CreateWellKnownSid`,
  `DeriveCapabilitySidsFromName`, `FreeSid`, `EqualSid`, `GetAce`, `GetAclInformation`, the ACE
  types and flags. `CreateProcessW`, `CreateJobObjectW` and `CreateMutexW` are also behind it
  (they take `SECURITY_ATTRIBUTES`). `Win32_Security_Authorization` already implies it, but name
  it.
- `Win32_Security_Isolation`: `CreateAppContainerProfile`, `DeriveAppContainerSidFromAppContainerName`.
- `Win32_Storage_FileSystem`: `FILE_GENERIC_READ`, `FILE_GENERIC_EXECUTE`, `FILE_GENERIC_WRITE`,
  `FILE_ALL_ACCESS`.
- `Win32_System_Console`: `GetStdHandle`, `STD_*_HANDLE`, `SetConsoleCtrlHandler`, `CTRL_*_EVENT`.
- `Win32_System_JobObjects`: the job object and its limits (`JOBOBJECT_EXTENDED_LIMIT_INFORMATION`
  also needs `Win32_System_Threading`, already there).

`Win32_System_Memory`, `Win32_System_Environment` and `Win32_UI_WindowsAndMessaging` are not
needed (`LocalFree` is in `Win32_Foundation`; the environment block is built by hand).

> forwarded to cargo: `crates/blaze-cargo/Cargo.toml` is lane cargo's; line 154 still has the five old features.

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/Cargo.toml:160, `windows-sys = { workspace = true, features = ["Win32_Security_Authorization", "Win32_Security_Isolation", "Win32_System_Console"] }`: the root's 0.61.2 entry already has Foundation, WinSock, Security, Storage_FileSystem, JobObjects, SystemServices and Threading, so together they are the ten asked.

## 2. `blaze-usercode` as a dependency of `blaze-cargo`

`windows.rs` takes `blaze_usercode::confine::Spec`, and `sandbox.rs` already uses
`blaze_usercode::trace`, but `crates/blaze-cargo/Cargo.toml` has no `blaze-usercode` dependency
yet (owner: `usercode-sandbox`, the parent). It also uses `getrandom` 0.3 (`getrandom::u64`),
which is already there.

> applied: `crates/blaze-cargo/Cargo.toml` names `blaze-usercode.workspace = true` (line 131) now.
