//! hyperion-abi: the frozen boundary header between the host and module dylibs (docs/host-diet.md 6 and 10.4): `Info`/`Entry`/`Step`, the
//! four-field `HostApi`, the entry-point types and the version constants. Zero dependencies. Layout, hash, component, service, manifest, panic
//! and cell code lives in `hyperion-{layout,wyhash,api-layouts,service,manifest-types,panic,cell}` (docs/host-diet.md 10.10).

pub mod table;

pub use table::*;
