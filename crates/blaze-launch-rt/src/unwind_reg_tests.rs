//! unwind_reg.rs's tests: the finder finds a pc's record (and nothing past it), the registry
//! refuses what would make a lookup wrong, and a placed record that a panic unwinds through runs
//! its landing pad and the panic reaches `catch_unwind` above it.

use blaze_own::{AVec, Alloc, SharedAlloc, Vec};

use super::{RegError, Registry, Span, find, insert, remove};

fn span(code_lo: u64, code_hi: u64) -> Span {
    Span { code_lo, code_hi, eh_at: code_lo + 0x10_0000, eh_len: 40, id: code_lo }
}

/// rule 45: the private-by-convention finder registration resolves in this process's libunwind.
/// Prints "unwind_reg: __unw_add_find_dynamic_unwind_sections resolves". Fails on a system
/// without it, where `install` refuses with `RegError::Missing` (never another route).
#[cfg(target_os = "macos")]
#[test]
fn the_dynamic_sections_finder_exists() {
    use blaze_sys::posix::thread::{RTLD_DEFAULT, symbol};

    // SAFETY: `dlsym` with `RTLD_DEFAULT` and a NUL-ended name only reads the name.
    let found = unsafe { symbol(RTLD_DEFAULT, c"__unw_add_find_dynamic_unwind_sections") };
    assert!(found.is_some(), "{}", RegError::Missing { symbol: super::FINDER_SYMBOL });
    println!("unwind_reg: {} resolves", super::FINDER_SYMBOL);
}

/// The lookup over sorted spans: a pc at a record's first byte and last byte finds it; its end,
/// a gap and a pc below everything find nothing; an overlapping or empty span is refused (a
/// lookup over overlapping spans could answer with the wrong entry: the planted case); a full
/// array is refused rather than grown; a removed span is no longer found. Prints "unwind_reg:
/// the finder finds a pc's row".
#[test]
fn the_finder_finds_a_pcs_row() {
    let mut spans: AVec<'_, Span> = Vec::with_capacity_in(3, Alloc::global());
    insert(&mut spans, span(0x2000, 0x2040)).expect("first");
    insert(&mut spans, span(0x1000, 0x1100)).expect("second, before the first");
    insert(&mut spans, span(0x3000, 0x3004)).expect("third");
    assert_eq!(find(&spans, 0x1000).map(|s| s.code_lo), Some(0x1000));
    assert_eq!(find(&spans, 0x10ff).map(|s| s.code_lo), Some(0x1000));
    assert_eq!(find(&spans, 0x1100), None, "a record's end is not in it");
    assert_eq!(find(&spans, 0x1800), None, "a gap");
    assert_eq!(find(&spans, 0x0fff), None, "below every record");
    assert_eq!(find(&spans, 0x2020).map(|s| s.eh_at), Some(0x2000 + 0x10_0000));
    assert_eq!(find(&spans, 0x3003).map(|s| s.code_lo), Some(0x3000));
    assert_eq!(find(&spans, u64::MAX), None);
    // Planted: an overlap with either neighbour, and an empty span.
    assert_eq!(insert(&mut spans, span(0x10f0, 0x1200)), Err(RegError::Overlaps { code_lo: 0x10f0, other_lo: 0x1000 }));
    assert_eq!(insert(&mut spans, span(0x1f00, 0x2001)), Err(RegError::Overlaps { code_lo: 0x1f00, other_lo: 0x2000 }));
    assert_eq!(insert(&mut spans, span(0x5000, 0x5000)), Err(RegError::Empty { code_lo: 0x5000, code_hi: 0x5000 }));
    assert_eq!(insert(&mut spans, span(0x6000, 0x6010)), Err(RegError::Full { cap: 3 }));
    assert_eq!(remove(&mut spans, 0x2000).map(|s| s.code_hi), Ok(0x2040));
    assert_eq!(find(&spans, 0x2020), None);
    assert_eq!(remove(&mut spans, 0x2000), Err(RegError::NotRegistered { code_lo: 0x2000 }));

    // The registry gives the same answers through its two sides, edit after edit.
    let registry = Registry::new_in(4, SharedAlloc::global()).expect("registry");
    let mut writer = registry.writer().expect("the one writer");
    assert_eq!(registry.writer().err(), Some(RegError::WriterTaken));
    // The registry's own edit (what `add` and `remove` run before telling the system): these
    // spans name no real entry, so they are never told to an unwinder.
    for (lo, hi) in [(0x2000, 0x2040), (0x1000, 0x1100), (0x3000, 0x3004)] {
        writer.edit(lo, |spans| insert(spans, span(lo, hi))).expect("add");
    }
    assert_eq!(registry.lookup(0x10ff).map(|s| s.code_lo), Some(0x1000));
    assert_eq!(registry.lookup(0x2040), None);
    assert_eq!(registry.lookup(0x3000).map(|s| s.code_lo), Some(0x3000));
    writer.edit(0x1000, |spans| remove(spans, 0x1000)).expect("remove");
    assert_eq!(registry.lookup(0x1000), None);
    assert_eq!(registry.lookup(0x2000).map(|s| s.code_lo), Some(0x2000));
    assert_eq!(registry.missed(), 0);
    drop(writer);
    assert!(registry.writer().is_ok(), "the writer is given back on drop");
    println!("unwind_reg: the finder finds a pc's row");
}

#[cfg(target_arch = "aarch64")]
mod placed {
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use blaze_code::eh::Placed;
    use blaze_code::unwind::{Action, Frame, Row};
    use blaze_own::{AVec, Alloc, SharedAlloc, Vec};
    use blaze_tier::Emitted;

    use super::super::{Registry, Span, eh_bytes};

    /// Set by the placed record's landing pad (a `strb` of 1 through the literal at 48).
    static LANDED: AtomicBool = AtomicBool::new(false);
    /// The LSDA address the personality was handed for the placed frame.
    static LSDA_SEEN: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        fn _Unwind_GetIP(context: *mut c_void) -> usize;
        fn _Unwind_GetRegionStart(context: *mut c_void) -> usize;
        fn _Unwind_GetLanguageSpecificData(context: *mut c_void) -> usize;
        fn _Unwind_SetIP(context: *mut c_void, ip: usize);
        fn _Unwind_SetGR(context: *mut c_void, reg: c_int, value: usize);
    }

    unsafe extern "C-unwind" {
        fn _Unwind_Resume(exception: *mut c_void) -> !;
    }

    const SEARCH_PHASE: c_int = 1;
    const INSTALL_CONTEXT: c_int = 7;
    const CONTINUE_UNWIND: c_int = 8;
    /// The landing pad's offset in the placed code.
    const LANDING: u32 = 20;

    /// The placed record's personality routine: the test's own, so the test does not depend on
    /// the name rustc gives std's. Search phase: no handler here. Cleanup phase at the call at 8:
    /// land at [`LANDING`] with the exception in x0 (what `_Unwind_Resume` is given); anywhere
    /// else (the call of `_Unwind_Resume`, a row with no landing): go on.
    unsafe extern "C" fn personality(_version: c_int, actions: c_int, _class: u64, exception: *mut c_void, context: *mut c_void) -> c_int {
        if actions & SEARCH_PHASE != 0 {
            return CONTINUE_UNWIND;
        }
        // SAFETY: `context` is the unwinder's for this frame, valid during the call.
        let (ip, start, lsda) = unsafe { (_Unwind_GetIP(context), _Unwind_GetRegionStart(context), _Unwind_GetLanguageSpecificData(context)) };
        let call = ip.wrapping_sub(start).wrapping_sub(1);
        if !(8..12).contains(&call) {
            return CONTINUE_UNWIND;
        }
        LSDA_SEEN.store(lsda, Ordering::SeqCst);
        // SAFETY: as above; x0 and x1 are the landing pad's inputs, the IP its address.
        unsafe {
            _Unwind_SetGR(context, 0, exception as usize);
            _Unwind_SetGR(context, 1, 0);
            _Unwind_SetIP(context, start + LANDING as usize);
        }
        INSTALL_CONTEXT
    }

    extern "C-unwind" fn boom() {
        panic!("a panic below a placed record (unwind_reg_tests.rs)");
    }

    /// The placed function, `fn(f: extern "C-unwind" fn())`: it calls `f` with a cleanup row
    /// landing at 20, where it sets [`LANDED`] and resumes the unwind.
    fn code(landed: u64, resume: u64) -> [u8; 64] {
        let words: [u32; 12] = [
            0xA9BF_7BFD, // 0: stp x29, x30, [sp, #-16]!
            0x9100_03FD, // 4: mov x29, sp
            0xD63F_0000, // 8: blr x0              (row: cleanup, landing 20)
            0xA8C1_7BFD, // 12: ldp x29, x30, [sp], #16
            0xD65F_03C0, // 16: ret
            0x5800_00E9, // 20: ldr x9, #48        (the landing: &LANDED)
            0x5280_002A, // 24: mov w10, #1
            0x3900_012A, // 28: strb w10, [x9]
            0x5800_00D0, // 32: ldr x16, #56       (_Unwind_Resume)
            0xD63F_0200, // 36: blr x16            (row: continue)
            0xD420_0000, // 40: brk #0             (never reached: _Unwind_Resume does not return)
            0xD503_201F, // 44: nop
        ];
        let mut out = [0u8; 64];
        for (i, word) in words.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
        out[48..56].copy_from_slice(&landed.to_le_bytes());
        out[56..64].copy_from_slice(&resume.to_le_bytes());
        out
    }

    /// A record placed at run time with a cleanup row is unwound through by a panic raised below
    /// it: the unwinder finds its entry through the registry (on macOS, the finder answers for
    /// its pc first), its landing pad runs (LANDED), the personality is handed the LSDA eh.rs
    /// wrote, and the panic reaches `catch_unwind` above it. Without the registration the
    /// unwinder finds no entry and the process aborts (the cause, removed: a separate process;
    /// here the finder is asked before `add` and answers nothing). Prints "unwind_reg: a panic
    /// below a placed record ran its landing pad and reached catch_unwind".
    #[test]
    fn a_placed_record_that_panics_unwinds_through_its_landing_pad() {
        // The only test that installs: the system keeps one finder of ours for the process.
        let made = Registry::new_in(16, SharedAlloc::global()).expect("registry");
        let registry: &'static Registry = std::boxed::Box::leak(std::boxed::Box::new(made));
        #[cfg(target_os = "macos")]
        super::super::install(registry).expect("the finder installs");
        let page = crate::sys::page().expect("page size");
        let mut area = crate::sys::Code::map(page).expect("a code area");
        let base = area.base() as u64;
        let bytes = code(core::ptr::from_ref(&LANDED) as u64, _Unwind_Resume as usize as u64);

        let mut record = Emitted::new_in(Alloc::global());
        record.code.extend_from_slice(&bytes);
        record.unwind_frame = Some(Frame::PLAIN);
        record.unwind.push(Row { start: 8, len: 4, landing: LANDING, action: Action::Cleanup });
        record.unwind.push(Row { start: 36, len: 4, landing: 0, action: Action::Continue });

        let mut eh: AVec<'_, u8> = Vec::with_capacity_in(1024, Alloc::global());
        let eh_at = eh.as_ptr() as u64;
        let at = Placed { entry_at: eh_at, code_at: base, personality: Some(personality as usize as u64) };
        let made = eh_bytes(&record, at, &mut eh).expect("the entry");
        assert_eq!(eh.as_ptr() as u64, eh_at, "the entry's bytes stayed where they were made for");
        let lsda = eh_at + made.entry.lsda.expect("a row lands, so there is an LSDA") as u64;

        area.write(0, &bytes).expect("the code written");
        let span = Span { code_lo: base, code_hi: base + bytes.len() as u64, eh_at, eh_len: made.eh_len, id: 1 };
        #[cfg(target_os = "macos")]
        assert!(super::super::macos::call_find(base + 8).is_none(), "nothing found before the record is added");
        let mut writer = registry.writer().expect("the one writer");
        // SAFETY: the entry's bytes are in `eh` and the code in `area`, both alive and unchanged
        // until `remove` below returns.
        unsafe { writer.add(span) }.expect("registered");
        #[cfg(target_os = "macos")]
        {
            let found = super::super::macos::call_find(base + 12).expect("the finder finds the call's return address");
            assert_eq!(found.map(|word| word as u64), [base, eh_at, made.eh_len, 0, 0], "dso_base, the .eh_frame part, its length, no compact unwind");
        }

        LANDED.store(false, Ordering::SeqCst);
        // SAFETY: `base` holds the function `code` wrote: AAPCS64, one argument, a frame record,
        // returns nothing; it unwinds only through its registered entry.
        let placed: extern "C-unwind" fn(extern "C-unwind" fn()) = unsafe { core::mem::transmute::<usize, extern "C-unwind" fn(extern "C-unwind" fn())>(base as usize) };
        let caught = std::panic::catch_unwind(|| placed(boom));
        assert!(caught.is_err(), "the panic reached catch_unwind above the placed record");
        assert!(LANDED.load(Ordering::SeqCst), "the placed record's landing pad ran");
        assert_eq!(LSDA_SEEN.load(Ordering::SeqCst) as u64, lsda, "the personality was handed eh.rs's LSDA");

        // SAFETY: no thread is in the placed code (the call returned by unwinding).
        unsafe { writer.remove(base) }.expect("taken back");
        assert_eq!(registry.lookup(base + 8), None);
        drop(eh);
        println!("unwind_reg: a panic below a placed record ran its landing pad and reached catch_unwind");
    }
}
