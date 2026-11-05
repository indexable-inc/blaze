//! NASM's standard macros.
//!
//! In NASM the user-level spellings of most directives are macros around a
//! bracketed primitive: `section .data` defines `__SECT__` as
//! `[section .data]` and then expands it, which is how `endstruc` knows where
//! to go back to. They are written here as NASM source, read before the first
//! file, so that they expand exactly as NASM's do — including in the ways
//! that show, such as `align` padding with one-byte `nop`s and `%0` counting
//! a `global`'s names. The behaviour follows NASM 2.16.03's `standard.mac`
//! and its bin and ELF output macros.

/// The macros every NASM source starts with.
pub(crate) const COMMON: &str = r#"
%define __?SECT?__ [section .text]
%defalias __SECT__ __?SECT?__

%imacro section 1+.nolist
  %define __?SECT?__ [section %1]
  __?SECT?__
%endmacro
%imacro segment 1+.nolist
  %define __?SECT?__ [segment %1]
  __?SECT?__
%endmacro
%imacro absolute 1+.nolist
  %define __?SECT?__ [absolute %1]
  __?SECT?__
%endmacro

%define __?SECTALIGN_ALIGN_UPDATES_SECTION?__ 1
%defalias __SECTALIGN_ALIGN_UPDATES_SECTION__ __?SECTALIGN_ALIGN_UPDATES_SECTION?__
%imacro sectalign 1+.nolist
  %ifidni %1,off
    %define __?SECTALIGN_ALIGN_UPDATES_SECTION?__ 0
  %elifidni %1,on
    %define __?SECTALIGN_ALIGN_UPDATES_SECTION?__ 1
  %else
    [sectalign %1]
  %endif
%endmacro

%imacro struc 1-2.nolist 0
  %push
  %define %$strucname %1
  [absolute %2]
  %$strucname:
%endmacro
%imacro endstruc 0.nolist
  %{$strucname}_size equ ($-%$strucname)
  %pop
  __?SECT?__
%endmacro

%imacro istruc 1.nolist
  %push
  %define %$strucname %1
  %$strucstart:
%endmacro
%imacro at 1-2+.nolist
  %defstr %$member %1
  %substr %$member1 %$member 1
  %ifidn %$member1, '.'
    times (%$strucname%1-%$strucname)-($-%$strucstart) db 0
  %else
    times (%1-%$strucname)-($-%$strucstart) db 0
  %endif
  %2
%endmacro
%imacro iend 0.nolist
  times %{$strucname}_size-($-%$strucstart) db 0
  %pop
%endmacro

%imacro align 1-2+.nolist nop
  %if __?SECTALIGN_ALIGN_UPDATES_SECTION?__
    sectalign %1
  %endif
  times (((%1) - (($-$$) % (%1))) % (%1)) %2
%endmacro
%imacro alignb 1-2+.nolist
  %if __?SECTALIGN_ALIGN_UPDATES_SECTION?__
    sectalign %1
  %endif
  %ifempty %2
    resb (((%1) - (($-$$) % (%1))) % (%1))
  %else
    times (((%1) - (($-$$) % (%1))) % (%1)) %2
  %endif
%endmacro

%imacro bits 1+.nolist
  [bits %1]
%endmacro
%imacro use16 0.nolist
  [bits 16]
%endmacro
%imacro use32 0.nolist
  [bits 32]
%endmacro
%imacro use64 0.nolist
  [bits 64]
%endmacro

%imacro extern 1-*.nolist
  %rep %0
    [extern %1]
    %rotate 1
  %endrep
%endmacro
%imacro static 1-*.nolist
  %rep %0
    [static %1]
    %rotate 1
  %endrep
%endmacro
%imacro global 1-*.nolist
  %rep %0
    [global %1]
    %rotate 1
  %endrep
%endmacro
%imacro required 1-*.nolist
  %rep %0
    [required %1]
    %rotate 1
  %endrep
%endmacro
%imacro common 1-*.nolist
  %rep %0
    [common %1]
    %rotate 1
  %endrep
%endmacro

%imacro cpu 1+.nolist
  [cpu %1]
%endmacro
%imacro float 1-*.nolist
  %rep %0
    [float %1]
    %rotate 1
  %endrep
%endmacro
%imacro default 1+.nolist
  [default %1]
%endmacro
%imacro userel 0.nolist
  [default rel]
%endmacro
%imacro useabs 0.nolist
  [default abs]
%endmacro

%imacro incbin 1-2+.nolist 0
  %push
  %pathsearch %$dep %1
  %? %$dep,%2
  %pop
%endmacro

%defalias __FILE__ __?FILE?__
%defalias __LINE__ __?LINE?__
%defalias __BITS__ __?BITS?__
%defalias __PTR__ __?PTR?__
%defalias __PASS__ __?PASS?__
%defalias __OUTPUT_FORMAT__ __?OUTPUT_FORMAT?__
%defalias __NASM_MAJOR__ __?NASM_MAJOR?__
%defalias __NASM_MINOR__ __?NASM_MINOR?__
%defalias __NASM_SUBMINOR__ __?NASM_SUBMINOR?__
%defalias __NASM_PATCHLEVEL__ __?NASM_PATCHLEVEL?__
%defalias __NASM_VERSION_ID__ __?NASM_VERSION_ID?__
%defalias __NASM_VER__ __?NASM_VER?__
"#;

/// The macros only a flat binary has.
pub(crate) const BIN: &str = r#"
%imacro org 1+.nolist
  [org %1]
%endmacro
"#;
