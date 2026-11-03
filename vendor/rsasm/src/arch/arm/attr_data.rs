//! What `.arch`, `.cpu`, `.fpu`, `.arch_extension`, `.object_arch`
//! and `.eabi_attribute` put in `.ARM.attributes`, measured from
//! `arm-none-eabi-as` by `tools/tables/arm-attrs.py`. Do not edit.
//!
//! A CPU's entry is every tag `.arch` or `.cpu` naming it leaves
//! behind, an FPU's is what `.fpu` puts in place of the tags a unit
//! decides, and an extension's is what adding it to that CPU
//! changed. See [`super::attrs`].

use super::attrs::{Cpu, Named};

/// The extensions one CPU takes, and what each changes.
static EXTS_0: &[Named] = &[
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_1: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_2: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "sec",
        tags: &[(6, 7), (68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_3: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_4: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_5: &[Named] = &[
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[(6, 12)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_6: &[Named] = &[
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_7: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[(44, 2)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[(42, 1)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[(44, 2), (68, 2)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_8: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_9: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[(44, 2)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[(42, 1)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_10: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_11: &[Named] = &[
    Named {
        key: "dsp",
        tags: &[(46, 1)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_12: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7), (12, 4)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_13: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_14: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7)],
    },
    Named {
        key: "dotprod",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp16",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp16fml",
        tags: &[(10, 7)],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_15: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_16: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7)],
    },
    Named {
        key: "dotprod",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp16",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp16fml",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_17: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7)],
    },
    Named {
        key: "dotprod",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp",
        tags: &[(10, 7)],
    },
    Named {
        key: "fp16",
        tags: &[],
    },
    Named {
        key: "fp16fml",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[(10, 7)],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_18: &[Named] = &[
    Named {
        key: "iwmmxt",
        tags: &[],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_19: &[Named] = &[
    Named {
        key: "iwmmxt",
        tags: &[],
    },
    Named {
        key: "iwmmxt2",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_20: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_21: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "sec",
        tags: &[(6, 7), (68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_22: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_23: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[(44, 2)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[(44, 2), (68, 3)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_24: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[(44, 2)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[(42, 1)],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[(44, 2), (68, 3)],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_25: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[],
    },
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(12, 4)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_26: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[],
    },
    Named {
        key: "dotprod",
        tags: &[],
    },
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "fp16",
        tags: &[],
    },
    Named {
        key: "fp16fml",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_27: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[(44, 2)],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[(42, 1)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_28: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[(42, 1)],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_29: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[],
    },
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_30: &[Named] = &[
    Named {
        key: "dsp",
        tags: &[],
    },
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_31: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "os",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[(68, 1)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_32: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_33: &[Named] = &[
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[],
    },
    Named {
        key: "iwmmxt2",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The extensions one CPU takes, and what each changes.
static EXTS_34: &[Named] = &[
    Named {
        key: "crc",
        tags: &[],
    },
    Named {
        key: "crypto",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "fp",
        tags: &[],
    },
    Named {
        key: "idiv",
        tags: &[],
    },
    Named {
        key: "iwmmxt",
        tags: &[(11, 1)],
    },
    Named {
        key: "iwmmxt2",
        tags: &[(11, 2)],
    },
    Named {
        key: "mp",
        tags: &[],
    },
    Named {
        key: "pan",
        tags: &[],
    },
    Named {
        key: "predres",
        tags: &[],
    },
    Named {
        key: "ras",
        tags: &[],
    },
    Named {
        key: "rdma",
        tags: &[(10, 7), (12, 4)],
    },
    Named {
        key: "sb",
        tags: &[],
    },
    Named {
        key: "sec",
        tags: &[],
    },
    Named {
        key: "simd",
        tags: &[],
    },
    Named {
        key: "virt",
        tags: &[],
    },
    Named {
        key: "xscale",
        tags: &[],
    },
];

/// The tags an `.fpu` replaces, which are the unit's own.
pub(crate) static FP_TAGS: &[u8] = &[10, 12, 27, 36];

/// Every `.fpu` name, and the tags it leaves.
pub(crate) static FPUS: &[Named] = &[
    Named {
        key: "arm1020e",
        tags: &[(10, 2)],
    },
    Named {
        key: "arm1020t",
        tags: &[(10, 1)],
    },
    Named {
        key: "arm1136jf-s",
        tags: &[(10, 2)],
    },
    Named {
        key: "arm1136jfs",
        tags: &[(10, 2)],
    },
    Named {
        key: "crypto-neon-fp-armv8",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "crypto-neon-fp-armv8.1",
        tags: &[(10, 7), (12, 4)],
    },
    Named {
        key: "fp-armv8",
        tags: &[(10, 7)],
    },
    Named {
        key: "fpv4-sp-d16",
        tags: &[(10, 6), (27, 1)],
    },
    Named {
        key: "fpv5-d16",
        tags: &[(10, 8)],
    },
    Named {
        key: "fpv5-sp-d16",
        tags: &[(10, 8), (27, 1)],
    },
    Named {
        key: "neon",
        tags: &[(10, 3), (12, 1)],
    },
    Named {
        key: "neon-fp-armv8",
        tags: &[(10, 7), (12, 3)],
    },
    Named {
        key: "neon-fp-armv8.1",
        tags: &[(10, 7), (12, 4)],
    },
    Named {
        key: "neon-fp16",
        tags: &[(10, 3), (12, 1), (36, 1)],
    },
    Named {
        key: "neon-vfpv3",
        tags: &[(10, 3), (12, 1)],
    },
    Named {
        key: "neon-vfpv4",
        tags: &[(10, 5), (12, 2)],
    },
    Named {
        key: "softfpa",
        tags: &[],
    },
    Named {
        key: "softvfp",
        tags: &[],
    },
    Named {
        key: "softvfp+vfp",
        tags: &[(10, 2)],
    },
    Named {
        key: "vfp",
        tags: &[(10, 2)],
    },
    Named {
        key: "vfp10",
        tags: &[(10, 2)],
    },
    Named {
        key: "vfp10-r0",
        tags: &[(10, 1)],
    },
    Named {
        key: "vfp3",
        tags: &[(10, 3)],
    },
    Named {
        key: "vfp9",
        tags: &[(10, 2)],
    },
    Named {
        key: "vfpv2",
        tags: &[(10, 2)],
    },
    Named {
        key: "vfpv3",
        tags: &[(10, 3)],
    },
    Named {
        key: "vfpv3-d16",
        tags: &[(10, 4)],
    },
    Named {
        key: "vfpv3-d16-fp16",
        tags: &[(10, 4), (36, 1)],
    },
    Named {
        key: "vfpv3-fp16",
        tags: &[(10, 3), (36, 1)],
    },
    Named {
        key: "vfpv3xd",
        tags: &[(10, 4), (27, 1)],
    },
    Named {
        key: "vfpv3xd-fp16",
        tags: &[(10, 4), (27, 1), (36, 1)],
    },
    Named {
        key: "vfpv4",
        tags: &[(10, 5)],
    },
    Named {
        key: "vfpv4-d16",
        tags: &[(10, 6)],
    },
    Named {
        key: "vfpxd",
        tags: &[(10, 1), (27, 1)],
    },
];

/// The names `.eabi_attribute` takes for a tag, lowercased,
/// and the tag each names.
pub(crate) static TAG_NAMES: &[(&str, u32)] = &[
    ("tag_abi_fp_16bit_format", 38),
    ("tag_abi_fp_denormal", 20),
    ("tag_abi_fp_exceptions", 21),
    ("tag_abi_fp_number_model", 23),
    ("tag_abi_fp_optimization_goals", 31),
    ("tag_abi_fp_rounding", 19),
    ("tag_abi_fp_user_exceptions", 22),
    ("tag_abi_hardfp_use", 27),
    ("tag_abi_pcs_got_use", 17),
    ("tag_abi_pcs_r9_use", 14),
    ("tag_abi_pcs_ro_data", 16),
    ("tag_abi_pcs_rw_data", 15),
    ("tag_abi_pcs_wchar_t", 18),
    ("tag_abi_vfp_args", 28),
    ("tag_abi_wmmx_args", 29),
    ("tag_abi_align8_needed", 24),
    ("tag_abi_align8_preserved", 25),
    ("tag_abi_align_needed", 24),
    ("tag_abi_align_preserved", 25),
    ("tag_abi_enum_size", 26),
    ("tag_abi_optimization_goals", 30),
    ("tag_arm_isa_use", 8),
    ("tag_advanced_simd_arch", 12),
    ("tag_bti_extension", 52),
    ("tag_bti_use", 74),
    ("tag_cpu_arch", 6),
    ("tag_cpu_arch_profile", 7),
    ("tag_cpu_unaligned_access", 34),
    ("tag_div_use", 44),
    ("tag_dsp_extension", 46),
    ("tag_fp_hp_extension", 36),
    ("tag_fp_arch", 10),
    ("tag_mpextension_use", 42),
    ("tag_mve_arch", 48),
    ("tag_pacret_use", 76),
    ("tag_pac_extension", 50),
    ("tag_pcs_config", 13),
    ("tag_t2ee_use", 66),
    ("tag_thumb_isa_use", 9),
    ("tag_vfp_hp_extension", 36),
    ("tag_vfp_arch", 10),
    ("tag_virtualization_use", 68),
    ("tag_wmmx_arch", 11),
    ("tag_nodefaults", 64),
];

/// Every architecture `.arch` names.
pub(crate) static ARCHS: &[Cpu] = &[
    Cpu {
        key: "armv1",
        cpu_name: "1",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv2",
        cpu_name: "2",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv2a",
        cpu_name: "2A",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv2s",
        cpu_name: "2S",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv3",
        cpu_name: "3",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv3m",
        cpu_name: "3M",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv4",
        cpu_name: "4",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv4xm",
        cpu_name: "4XM",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv4t",
        cpu_name: "4T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv4txm",
        cpu_name: "4TXM",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv5",
        cpu_name: "5",
        tags: &[(6, 3), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv5t",
        cpu_name: "5T",
        tags: &[(6, 3), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv5txm",
        cpu_name: "5TXM",
        tags: &[(6, 3), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "armv5te",
        cpu_name: "5TE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv5texp",
        cpu_name: "5TEXP",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv5tej",
        cpu_name: "5TEJ",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv6",
        cpu_name: "6",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv6j",
        cpu_name: "6J",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv6k",
        cpu_name: "6K",
        tags: &[(6, 9), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_2,
    },
    Cpu {
        key: "armv6z",
        cpu_name: "6Z",
        tags: &[(6, 7), (8, 1), (9, 1), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_3,
    },
    Cpu {
        key: "armv6kz",
        cpu_name: "6KZ",
        tags: &[(6, 7), (8, 1), (9, 1), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_3,
    },
    Cpu {
        key: "armv6zk",
        cpu_name: "6ZK",
        tags: &[(6, 7), (8, 1), (9, 1), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_3,
    },
    Cpu {
        key: "armv6t2",
        cpu_name: "6T2",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv6kt2",
        cpu_name: "6KT2",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_4,
    },
    Cpu {
        key: "armv6zt2",
        cpu_name: "6ZT2",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_1,
    },
    Cpu {
        key: "armv6kzt2",
        cpu_name: "6KZT2",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_3,
    },
    Cpu {
        key: "armv6zkt2",
        cpu_name: "6ZKT2",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_3,
    },
    Cpu {
        key: "armv6-m",
        cpu_name: "6-M",
        tags: &[(6, 11), (7, 77), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_5,
    },
    Cpu {
        key: "armv6s-m",
        cpu_name: "6S-M",
        tags: &[(6, 12), (7, 77), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_6,
    },
    Cpu {
        key: "armv7",
        cpu_name: "7",
        tags: &[(6, 10), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_4,
    },
    Cpu {
        key: "armv7a",
        cpu_name: "7A",
        tags: &[(6, 10), (7, 65), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_7,
    },
    Cpu {
        key: "armv7ve",
        cpu_name: "7VE",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (44, 2),
            (68, 3),
        ],
        exts: EXTS_8,
    },
    Cpu {
        key: "armv7r",
        cpu_name: "7R",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_9,
    },
    Cpu {
        key: "armv7m",
        cpu_name: "7M",
        tags: &[(6, 10), (7, 77), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_6,
    },
    Cpu {
        key: "armv7-a",
        cpu_name: "7-A",
        tags: &[(6, 10), (7, 65), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_7,
    },
    Cpu {
        key: "armv7-r",
        cpu_name: "7-R",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_9,
    },
    Cpu {
        key: "armv7-m",
        cpu_name: "7-M",
        tags: &[(6, 10), (7, 77), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_6,
    },
    Cpu {
        key: "armv7e-m",
        cpu_name: "7E-M",
        tags: &[(6, 13), (7, 77), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_10,
    },
    Cpu {
        key: "armv8-m.base",
        cpu_name: "8-M.BASE",
        tags: &[(6, 16), (7, 77), (9, 3), (10, 5), (12, 2)],
        exts: EXTS_6,
    },
    Cpu {
        key: "armv8-m.main",
        cpu_name: "8-M.MAIN",
        tags: &[(6, 17), (7, 77), (9, 3), (10, 5), (12, 2)],
        exts: EXTS_11,
    },
    Cpu {
        key: "armv8.1-m.main",
        cpu_name: "8.1-M.MAIN",
        tags: &[(6, 21), (7, 77), (9, 3), (10, 5), (12, 2)],
        exts: EXTS_11,
    },
    Cpu {
        key: "armv8-a",
        cpu_name: "8-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_12,
    },
    Cpu {
        key: "armv8.1-a",
        cpu_name: "8.1-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_13,
    },
    Cpu {
        key: "armv8.2-a",
        cpu_name: "8.2-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_14,
    },
    Cpu {
        key: "armv8.3-a",
        cpu_name: "8.3-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_14,
    },
    Cpu {
        key: "armv8-r",
        cpu_name: "8-R",
        tags: &[
            (6, 15),
            (7, 82),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_15,
    },
    Cpu {
        key: "armv8.4-a",
        cpu_name: "8.4-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_16,
    },
    Cpu {
        key: "armv8.5-a",
        cpu_name: "8.5-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_16,
    },
    Cpu {
        key: "armv8.6-a",
        cpu_name: "8.6-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv8.7-a",
        cpu_name: "8.7-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv8.8-a",
        cpu_name: "8.8-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv8.9-a",
        cpu_name: "8.9-A",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv9-a",
        cpu_name: "9-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_16,
    },
    Cpu {
        key: "armv9.1-a",
        cpu_name: "9.1-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv9.2-a",
        cpu_name: "9.2-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv9.3-a",
        cpu_name: "9.3-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv9.4-a",
        cpu_name: "9.4-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "armv9.5-a",
        cpu_name: "9.5-A",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_17,
    },
    Cpu {
        key: "xscale",
        cpu_name: "xscale",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_0,
    },
    Cpu {
        key: "iwmmxt",
        cpu_name: "iwmmxt",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (11, 1), (12, 2)],
        exts: EXTS_18,
    },
    Cpu {
        key: "iwmmxt2",
        cpu_name: "iwmmxt2",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (11, 2), (12, 2)],
        exts: EXTS_19,
    },
];

/// Every architecture `.cpu` names.
pub(crate) static CPUS: &[Cpu] = &[
    Cpu {
        key: "arm1",
        cpu_name: "ARM1",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm2",
        cpu_name: "ARM2",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm250",
        cpu_name: "ARM250",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm3",
        cpu_name: "ARM3",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm6",
        cpu_name: "ARM6",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm60",
        cpu_name: "ARM60",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm600",
        cpu_name: "ARM600",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm610",
        cpu_name: "ARM610",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm620",
        cpu_name: "ARM620",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7",
        cpu_name: "ARM7",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7m",
        cpu_name: "ARM7M",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7d",
        cpu_name: "ARM7D",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7dm",
        cpu_name: "ARM7DM",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7di",
        cpu_name: "ARM7DI",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7dmi",
        cpu_name: "ARM7DMI",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm70",
        cpu_name: "ARM70",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm700",
        cpu_name: "ARM700",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm700i",
        cpu_name: "ARM700I",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm710",
        cpu_name: "ARM710",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm710t",
        cpu_name: "ARM710T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm720",
        cpu_name: "ARM720",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm720t",
        cpu_name: "ARM720T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm740t",
        cpu_name: "ARM740T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm710c",
        cpu_name: "ARM710C",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7100",
        cpu_name: "ARM7100",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7500",
        cpu_name: "ARM7500",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7500fe",
        cpu_name: "ARM7500FE",
        tags: &[(8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7t",
        cpu_name: "ARM7T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7tdmi",
        cpu_name: "ARM7TDMI",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm7tdmi-s",
        cpu_name: "ARM7TDMI-S",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm8",
        cpu_name: "ARM8",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm810",
        cpu_name: "ARM810",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "strongarm",
        cpu_name: "STRONGARM",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "strongarm1",
        cpu_name: "STRONGARM1",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "strongarm110",
        cpu_name: "STRONGARM110",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "strongarm1100",
        cpu_name: "STRONGARM1100",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "strongarm1110",
        cpu_name: "STRONGARM1110",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm9",
        cpu_name: "ARM9",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm920",
        cpu_name: "ARM920T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm920t",
        cpu_name: "ARM920T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm922t",
        cpu_name: "ARM922T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm940t",
        cpu_name: "ARM940T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm9tdmi",
        cpu_name: "ARM9TDMI",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa526",
        cpu_name: "FA526",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa626",
        cpu_name: "FA626",
        tags: &[(6, 1), (8, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm9e-r0",
        cpu_name: "ARM9E-R0",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm9e",
        cpu_name: "ARM9E",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm926ej",
        cpu_name: "ARM926EJ-S",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm926ejs",
        cpu_name: "ARM926EJ-S",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm926ej-s",
        cpu_name: "ARM926EJ-S",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm946e-r0",
        cpu_name: "ARM946E-R0",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm946e",
        cpu_name: "ARM946E-S",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm946e-s",
        cpu_name: "ARM946E-S",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm966e-r0",
        cpu_name: "ARM966E-R0",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm966e",
        cpu_name: "ARM966E-S",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm966e-s",
        cpu_name: "ARM966E-S",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm968e-s",
        cpu_name: "ARM968E-S",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm10t",
        cpu_name: "ARM10T",
        tags: &[(6, 3), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm10tdmi",
        cpu_name: "ARM10TDMI",
        tags: &[(6, 3), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm10e",
        cpu_name: "ARM10E",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1020",
        cpu_name: "ARM1020E",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1020t",
        cpu_name: "ARM1020T",
        tags: &[(6, 3), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1020e",
        cpu_name: "ARM1020E",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1022e",
        cpu_name: "ARM1022E",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1026ejs",
        cpu_name: "ARM1026EJ-S",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1026ej-s",
        cpu_name: "ARM1026EJ-S",
        tags: &[(6, 5), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa606te",
        cpu_name: "FA606TE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa616te",
        cpu_name: "FA616TE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa626te",
        cpu_name: "FA626TE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fmp626",
        cpu_name: "FMP626",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "fa726te",
        cpu_name: "FA726TE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1136js",
        cpu_name: "ARM1136J-S",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1136j-s",
        cpu_name: "ARM1136J-S",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1136jfs",
        cpu_name: "ARM1136JF-S",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1136jf-s",
        cpu_name: "ARM1136JF-S",
        tags: &[(6, 6), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "mpcore",
        cpu_name: "MPCore",
        tags: &[(6, 9), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_21,
    },
    Cpu {
        key: "mpcorenovfp",
        cpu_name: "MPCore",
        tags: &[(6, 9), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_21,
    },
    Cpu {
        key: "arm1156t2-s",
        cpu_name: "ARM1156T2-S",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1156t2f-s",
        cpu_name: "ARM1156T2F-S",
        tags: &[(6, 8), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "arm1176jz-s",
        cpu_name: "ARM1176JZ-S",
        tags: &[(6, 7), (8, 1), (9, 1), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_22,
    },
    Cpu {
        key: "arm1176jzf-s",
        cpu_name: "ARM1176JZF-S",
        tags: &[(6, 7), (8, 1), (9, 1), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_22,
    },
    Cpu {
        key: "cortex-a5",
        cpu_name: "Cortex-A5",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 1),
        ],
        exts: EXTS_23,
    },
    Cpu {
        key: "cortex-a7",
        cpu_name: "Cortex-A7",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (44, 2),
            (68, 3),
        ],
        exts: EXTS_8,
    },
    Cpu {
        key: "cortex-a8",
        cpu_name: "Cortex-A8",
        tags: &[(6, 10), (7, 65), (8, 1), (9, 2), (10, 5), (12, 2), (68, 1)],
        exts: EXTS_24,
    },
    Cpu {
        key: "cortex-a9",
        cpu_name: "Cortex-A9",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 1),
        ],
        exts: EXTS_23,
    },
    Cpu {
        key: "cortex-a12",
        cpu_name: "Cortex-A12",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (44, 2),
            (68, 3),
        ],
        exts: EXTS_8,
    },
    Cpu {
        key: "cortex-a15",
        cpu_name: "Cortex-A15",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (44, 2),
            (68, 3),
        ],
        exts: EXTS_8,
    },
    Cpu {
        key: "cortex-a17",
        cpu_name: "Cortex-A17",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (44, 2),
            (68, 3),
        ],
        exts: EXTS_8,
    },
    Cpu {
        key: "cortex-a32",
        cpu_name: "Cortex-A32",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a35",
        cpu_name: "Cortex-A35",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a53",
        cpu_name: "Cortex-A53",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a55",
        cpu_name: "Cortex-A55",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a57",
        cpu_name: "Cortex-A57",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a72",
        cpu_name: "Cortex-A72",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a73",
        cpu_name: "Cortex-A73",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "cortex-a75",
        cpu_name: "Cortex-A75",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a76",
        cpu_name: "Cortex-A76",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a76ae",
        cpu_name: "Cortex-A76AE",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a77",
        cpu_name: "Cortex-A77",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a78",
        cpu_name: "Cortex-A78",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a78ae",
        cpu_name: "Cortex-A78AE",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a78c",
        cpu_name: "Cortex-A78C",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-r4",
        cpu_name: "Cortex-R4",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_27,
    },
    Cpu {
        key: "cortex-r4f",
        cpu_name: "Cortex-R4F",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_27,
    },
    Cpu {
        key: "cortex-r5",
        cpu_name: "Cortex-R5",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2), (44, 2)],
        exts: EXTS_28,
    },
    Cpu {
        key: "cortex-r7",
        cpu_name: "Cortex-R7",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2), (44, 2)],
        exts: EXTS_28,
    },
    Cpu {
        key: "cortex-r8",
        cpu_name: "Cortex-R8",
        tags: &[(6, 10), (7, 82), (8, 1), (9, 2), (10, 5), (12, 2), (44, 2)],
        exts: EXTS_28,
    },
    Cpu {
        key: "cortex-r52",
        cpu_name: "Cortex-R52",
        tags: &[
            (6, 15),
            (7, 82),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_29,
    },
    Cpu {
        key: "cortex-r52plus",
        cpu_name: "Cortex-R52+",
        tags: &[
            (6, 15),
            (7, 82),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_29,
    },
    Cpu {
        key: "cortex-m85",
        cpu_name: "Cortex-M85",
        tags: &[(6, 21), (7, 77), (9, 3), (10, 7), (12, 2), (46, 1), (48, 2)],
        exts: EXTS_30,
    },
    Cpu {
        key: "cortex-m55",
        cpu_name: "Cortex-M55",
        tags: &[(6, 21), (7, 77), (9, 3), (10, 7), (12, 2), (46, 1), (48, 2)],
        exts: EXTS_30,
    },
    Cpu {
        key: "cortex-m52",
        cpu_name: "Cortex-M52",
        tags: &[(6, 21), (7, 77), (9, 3), (10, 7), (12, 2), (46, 1), (48, 2)],
        exts: EXTS_30,
    },
    Cpu {
        key: "cortex-m35p",
        cpu_name: "Cortex-M35P",
        tags: &[(6, 17), (7, 77), (9, 3), (10, 5), (12, 2), (46, 1)],
        exts: EXTS_30,
    },
    Cpu {
        key: "cortex-m33",
        cpu_name: "Cortex-M33",
        tags: &[(6, 17), (7, 77), (9, 3), (10, 5), (12, 2), (46, 1)],
        exts: EXTS_30,
    },
    Cpu {
        key: "cortex-m23",
        cpu_name: "Cortex-M23",
        tags: &[(6, 16), (7, 77), (9, 3), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m7",
        cpu_name: "Cortex-M7",
        tags: &[(6, 13), (7, 77), (9, 2), (10, 7), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m4",
        cpu_name: "Cortex-M4",
        tags: &[(6, 13), (7, 77), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m3",
        cpu_name: "Cortex-M3",
        tags: &[(6, 10), (7, 77), (9, 2), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m1",
        cpu_name: "Cortex-M1",
        tags: &[(6, 12), (7, 77), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m0",
        cpu_name: "Cortex-M0",
        tags: &[(6, 12), (7, 77), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-m0plus",
        cpu_name: "Cortex-M0+",
        tags: &[(6, 12), (7, 77), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_31,
    },
    Cpu {
        key: "cortex-x1",
        cpu_name: "Cortex-X1",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-x1c",
        cpu_name: "Cortex-X1C",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "exynos-m1",
        cpu_name: "Samsung Exynos M1",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "neoverse-n1",
        cpu_name: "Neoverse N1",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "ares",
        cpu_name: "Ares",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "neoverse-n2",
        cpu_name: "Neoverse N2",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "cortex-a710",
        cpu_name: "Cortex-A710",
        tags: &[
            (6, 22),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "neoverse-v1",
        cpu_name: "Neoverse V1",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 4),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_26,
    },
    Cpu {
        key: "xscale",
        cpu_name: "XSCALE",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "iwmmxt",
        cpu_name: "IWMMXT",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (11, 1), (12, 2)],
        exts: EXTS_32,
    },
    Cpu {
        key: "iwmmxt2",
        cpu_name: "IWMMXT2",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (11, 2), (12, 2)],
        exts: EXTS_33,
    },
    Cpu {
        key: "i80200",
        cpu_name: "I80200",
        tags: &[(6, 4), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "ep9312",
        cpu_name: "ARM920T",
        tags: &[(6, 2), (8, 1), (9, 1), (10, 5), (12, 2)],
        exts: EXTS_20,
    },
    Cpu {
        key: "marvell-pj4",
        cpu_name: "MARVELL-PJ4",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 1),
        ],
        exts: EXTS_23,
    },
    Cpu {
        key: "marvell-whitney",
        cpu_name: "MARVELL-WHITNEY",
        tags: &[
            (6, 10),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 1),
        ],
        exts: EXTS_23,
    },
    Cpu {
        key: "xgene1",
        cpu_name: "APM X-Gene 1",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 7),
            (12, 3),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_25,
    },
    Cpu {
        key: "xgene2",
        cpu_name: "APM X-Gene 2",
        tags: &[
            (6, 14),
            (7, 65),
            (8, 1),
            (9, 2),
            (10, 5),
            (12, 2),
            (42, 1),
            (68, 3),
        ],
        exts: EXTS_34,
    },
];
