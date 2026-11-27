//! `blaze jit-exec --exe <path> --arg0 <name> --manifest <hex> [-- <user args...>]`:
//! hidden entry point invoked by self-describing JIT-on-call executables produced by default
//! `blaze build`.
//!
//! Opens the pinned checked answer (`manifest.cert`) directly from the CAS store with ZERO
//! workspace stat sweep or re-checking, spawns `<exe>` itself with `BLAZE_LAUNCH_FD=3` so
//! `std::env::current_exe()` inside the program resolves to the built executable, JIT-compiles
//! only called functions on demand via the shared `HomeCodeStore`, persists the updated start
//! list and `BPG1` PGO profile on exit, and exits with the program's exact exit code.

use blaze_own::{Erased, Vec};
use bstd::Cx;
use bstd::args::Args;
use bstd::entry::Status;
use bstd::fs::Fs;
use bstd::out::Out;

use crate::engine_check::CheckScope;
use crate::launcher_stub::{JitBuildManifest, JitScopeKind};
use crate::own_process::OwnOverrides;
use crate::phases::Phases;
use crate::{CliError, program, run_package};

#[expect(clippy::too_many_arguments, reason = "top-level command entry: each argument one capability extracted at main")]
pub(crate) fn command(
    cx: &Cx,
    args: &Args,
    fs: &Fs,
    env: Option<&bstd::env::Env>,
    host: &bstd::host::Host,
    _clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let frame = cx.frame();
    let mut argv: Vec<&str, _> = Vec::with_capacity_in(args.len(), frame);
    argv.extend(args.each());
    let erased = Erased::new(frame);
    let parsed = match blaze_cli_spec::app::parse_argv(&blaze_cli_spec::blaze_app::BLAZE_APP, argv.as_slice(), erased.handle()) {
        Ok(blaze_cli_spec::app::ParseOutcome::Command(parsed)) => parsed,
        Ok(blaze_cli_spec::app::ParseOutcome::Help(_) | blaze_cli_spec::app::ParseOutcome::Version) => return Ok(Status::OK),
        Err(error) => return Err(crate::parse_error_to_cli(error)),
    };
    let exe = parsed
        .last_value("exe")
        .ok_or(CliError::Usage("`blaze jit-exec` requires --exe <path>"))?;
    let arg0 = parsed
        .last_value("arg0")
        .ok_or(CliError::Usage("`blaze jit-exec` requires --arg0 <name>"))?;
    let manifest_hex = parsed
        .last_value("manifest")
        .ok_or(CliError::Usage("`blaze jit-exec` requires --manifest <hex>"))?;

    let manifest = JitBuildManifest::decode_hex(manifest_hex, erased.handle()).map_err(|why| {
        CliError::Export(blaze_own::Box::new_in(
            crate::engine_export::ExportCliError::JitManifest {
                shown: crate::engine_export::owned_fmt_err(format_args!("{why}"), frame, heap.alloc()),
            },
            heap.alloc(),
        ))
    })?;

    let picked = run_package::Program {
        package: manifest.package.as_str(),
        bin: manifest.bin.as_str(),
        manifest: manifest.manifest.as_str(),
        root: manifest.root.as_str(),
        edition: manifest.edition.as_str(),
    };
    let scope = match manifest.scope_kind {
        JitScopeKind::Whole => CheckScope::Whole,
        JitScopeKind::Bin => CheckScope::Bin { package: picked.package, root: picked.root },
    };
    let cert = blaze_ids::ContentId(manifest.cert);
    let toolchain = blaze_toolchain::ToolchainId(manifest.toolchain);
    let pgo_key = blaze_cas::Key(blaze_ids::ContentId(manifest.pgo_key));

    let launch = program::Launch {
        fs,
        env,
        host,
        out,
        err,
        select: run_package::Select::NONE,
        open: blaze_run::viewer::OpenExplore::No,
        grants: &[],
        args: parsed.passthrough.as_slice(),
    };
    let overrides = OwnOverrides {
        launcher: Some(exe),
        arg0: Some(arg0),
        store_home: Some(manifest.store_home.as_str()),
        pgo_key: Some(pgo_key),
    };

    crate::engine_check::open_pinned_standing(
        cx,
        fs,
        env,
        host,
        rng,
        err,
        phases,
        heap,
        manifest.checkout.as_str(),
        manifest.store_home.as_str(),
        manifest.cargo_home.as_deref(),
        manifest.sdk_root.as_deref(),
        cert,
        toolchain,
        scope,
        picked,
        |standing, phases, _rng| crate::engine_start::run_jit_built(cx, standing, &picked, &launch, overrides, phases),
    )
}
