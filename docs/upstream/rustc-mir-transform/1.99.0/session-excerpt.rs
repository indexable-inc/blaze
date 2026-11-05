// Excerpts of compiler/rustc_session/src/session.rs at 1.99.0: what picks the MIR opt level,
// whether storage markers survive (lifetime markers) and whether UB checks are on.

    pub fn emit_lifetime_markers(&self) -> bool {
        self.opts.optimize != config::OptLevel::No
        // AddressSanitizer and KernelAddressSanitizer uses lifetimes to detect use after scope bugs.
        //
        // MemorySanitizer uses lifetimes to detect use of uninitialized stack variables.
        //
        // HWAddressSanitizer and KernelHWAddressSanitizer will use lifetimes to detect use after
        // scope bugs in the future.
        || self.sanitizers().intersects(SanitizerSet::ADDRESS | SanitizerSet::KERNELADDRESS | SanitizerSet::MEMORY | SanitizerSet::HWADDRESS | SanitizerSet::KERNELHWADDRESS)
        // Lifetimes are necessary for retagging semantics.
        || self.opts.unstable_opts.codegen_emit_retag.is_some()

    pub fn mir_opt_level(&self) -> usize {
        self.opts
            .unstable_opts
            .mir_opt_level
            .unwrap_or_else(|| if self.opts.optimize != OptLevel::No { 2 } else { 1 })
    }

    pub fn ub_checks(&self) -> bool {
        self.opts.unstable_opts.ub_checks.unwrap_or(self.opts.debug_assertions)
    }
