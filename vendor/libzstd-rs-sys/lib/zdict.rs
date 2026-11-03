#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct ZDICT_params_t {
    /// Choose which zstd compression level to optimize for.
    ///
    /// 0 means default ([`crate::ZSTD_CLEVEL_DEFAULT`]).
    pub compressionLevel: core::ffi::c_int,
    /// Choose which logs to write to stderr:
    ///
    /// - 0 none (default)
    /// - 1 errors
    /// - 2 progression
    /// - 3 details
    /// - 4 debug
    pub notificationLevel: core::ffi::c_uint,
    /// Force a `dictID` value.
    ///
    /// 0 means automatic mode (32-bits random value).
    ///
    /// Note: the zstd format reserves some dictionary IDs for future use. You may use them in
    /// private settings, but be warned that they may be used by zstd in a public dictionary
    /// registry in the future. The reserved dictionary IDs are:
    /// - low range: <= 32767
    /// - high range: >= (2^31)
    pub dictID: core::ffi::c_uint,
}

pub use crate::lib::dictBuilder::zdict::{
    ZDICT_finalizeDictionary, ZDICT_getDictHeaderSize, ZDICT_getDictID, ZDICT_getErrorName,
    ZDICT_isError, ZDICT_trainFromBuffer,
};

pub mod experimental {
    use super::ZDICT_params_t;

    pub const ZDICT_DICTSIZE_MIN: usize = 256;

    #[deprecated = "will be removed in v1.6.0"]
    pub const ZDICT_CONTENTSIZE_MIN: usize = 128;

    /// `k` and `d` are the only required parameters. For others, value 0 means default.
    #[derive(Debug, Copy, Clone, Default)]
    #[repr(C)]
    pub struct ZDICT_cover_params_t {
        /// Set the segment size `0 < k`. (reasonable range \[16, 2048+])
        pub k: core::ffi::c_uint,
        /// Set the dmer size `0 < d <= k`. (reasonable range \[6, 16])
        pub d: core::ffi::c_uint,
        /// Set the number of steps (only used for optimization).
        ///
        /// 0 means default (40), higher means more parameters checked.
        pub steps: core::ffi::c_uint,
        /// Set the number of threads `0 < nbThreads` (only used for optimization).
        ///
        /// 1 means single-threaded.
        pub nbThreads: core::ffi::c_uint,
        /// Set the percentage of samples used for training (only used for optimization).
        ///
        /// The first `nbSamples * splitPoint` samples will be used for training, the last
        /// `nbSamples * (1 - splitPoint)` samples will be used for testing.
        ///
        /// 0.0 means default (1.0), 1.0 means all samples are used for both training and testing.
        pub splitPoint: core::ffi::c_double,
        /// Train dictionaries to shrink in size starting from the minimum size and selects the
        /// smallest dictionary that is `shrinkDictMaxRegression%` worse than the largest dictionary.
        ///
        /// 0 means no shrinking and 1 means shrinking.
        pub shrinkDict: core::ffi::c_uint,
        /// Set the maximum regression so that a smaller dictionary can be no more than
        /// `shrinkDictMaxRegression%` worse than the max dict size dictionary.
        pub shrinkDictMaxRegression: core::ffi::c_uint,
        pub zParams: ZDICT_params_t,
    }

    #[derive(Debug, Copy, Clone, Default)]
    #[repr(C)]
    pub struct ZDICT_fastCover_params_t {
        /// Segment size `0 < k` (reasonable range \[16, 2048+])
        pub k: core::ffi::c_uint,
        /// dmer size `0 < d <= k` (reasonable range \[6, 16])
        pub d: core::ffi::c_uint,
        /// log of frequency array size `0 < f <= 31`
        ///
        /// 1 means default (20).
        pub f: core::ffi::c_uint,
        /// Number of steps (only used for optimization)
        ///
        /// 0 means default (40), higher means more parameters checked.
        pub steps: core::ffi::c_uint,
        /// Number of threads `0 < nbThreads` (only used for optimization)
        ///
        /// 1 means single-threaded.
        pub nbThreads: core::ffi::c_uint,
        /// Percentage of samples used for training (only used for optimization)
        ///
        /// The first `nbSamples * splitPoint` samples will be used for training, the last
        /// `nbSamples * (1 - splitPoint)` samples will be used for testing.
        ///
        /// 0.0 means default (0.75), 1.0 means all samples are used for both training and testing.
        pub splitPoint: core::ffi::c_double,
        /// Acceleration level `0 < accel <= 10`
        ///
        /// Higher means faster and less accurate, 0 means default (1).
        pub accel: core::ffi::c_uint,
        /// Train dictionaries to shrink in size starting from the minimum size and selects the
        /// smallest dictionary that is `shrinkDictMaxRegression%` worse than the largest dictionary.
        ///
        /// 0 means no shrinking and 1 means shrinking.
        pub shrinkDict: core::ffi::c_uint,
        /// Sets shrinkDictMaxRegression so that a smaller dictionary can be at worse
        /// `shrinkDictMaxRegression%` worse than the max dict size dictionary.
        pub shrinkDictMaxRegression: core::ffi::c_uint,
        pub zParams: ZDICT_params_t,
    }

    pub use crate::lib::dictBuilder::cover::{
        ZDICT_optimizeTrainFromBuffer_cover, ZDICT_trainFromBuffer_cover,
    };
    pub use crate::lib::dictBuilder::fastcover::{
        ZDICT_optimizeTrainFromBuffer_fastCover, ZDICT_trainFromBuffer_fastCover,
    };

    #[derive(Debug, Copy, Clone, Default)]
    #[repr(C)]
    pub struct ZDICT_legacy_params_t {
        /// 0 means default (9); larger means it will select more, so you will get a larger dictionary.
        pub selectivityLevel: core::ffi::c_uint,
        pub zParams: ZDICT_params_t,
    }

    #[expect(deprecated)]
    pub use crate::lib::dictBuilder::zdict::{
        ZDICT_addEntropyTablesFromBuffer, ZDICT_trainFromBuffer_legacy,
    };
}
