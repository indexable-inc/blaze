use core::marker::PhantomData;

use crate::lib::common::error_private::Error;
use crate::lib::common::mem::MEM_writeLEST;
use crate::lib::polyfill::unlikely;

pub(crate) type BitContainerType = usize;

pub(crate) const STREAM_ACCUMULATOR_MIN_32: u32 = 25;
pub(crate) const STREAM_ACCUMULATOR_MIN_64: u32 = 57;
pub(crate) const STREAM_ACCUMULATOR_MIN: u32 = match size_of::<usize>() {
    4 => STREAM_ACCUMULATOR_MIN_32,
    8 => STREAM_ACCUMULATOR_MIN_64,
    _ => unreachable!(),
};

#[repr(C)]
#[derive(Default)]
pub(crate) struct BIT_CStream_t {
    // FIXME make all fields private to this module
    pub(crate) bitContainer: BitContainerType,
    pub(crate) bitPos: core::ffi::c_uint,
    pub(crate) startPtr: *mut core::ffi::c_char,
    pub(crate) ptr: *mut core::ffi::c_char,
    pub(crate) endPtr: *mut core::ffi::c_char,
}

impl BIT_CStream_t {
    #[inline]
    pub(crate) fn new(startPtr: *mut core::ffi::c_void, dstCapacity: usize) -> Result<Self, Error> {
        if dstCapacity <= size_of::<BitContainerType>() {
            return Err(Error::dstSize_tooSmall);
        }

        let startPtr = startPtr as *mut core::ffi::c_char;
        Ok(BIT_CStream_t {
            bitContainer: 0,
            bitPos: 0,
            startPtr,
            ptr: startPtr,
            endPtr: startPtr
                .wrapping_add(dstCapacity)
                .wrapping_sub(size_of::<BitContainerType>()),
        })
    }

    /// Add up to 31 bits into the bit stream.
    ///
    /// Note: does not check for register overflow!
    #[inline]
    pub(crate) fn add_bits(&mut self, value: BitContainerType, nbBits: core::ffi::c_uint) {
        debug_assert!(nbBits < BIT_MASK_SIZE as u32);
        debug_assert!(nbBits + self.bitPos < BitContainerType::BITS);
        self.bitContainer |= BIT_getLowerBits(value, nbBits) << self.bitPos;
        self.bitPos = self.bitPos.wrapping_add(nbBits);
    }

    /// Works only if `value` is clean, meaning all high bits above `nbBits` are 0.
    #[inline]
    fn add_bits_fast(&mut self, value: BitContainerType, nbBits: core::ffi::c_uint) {
        debug_assert_eq!(value >> nbBits, 0);
        debug_assert!(nbBits + self.bitPos < BitContainerType::BITS);
        self.bitContainer |= value << self.bitPos;
        self.bitPos = self.bitPos.wrapping_add(nbBits);
    }

    /// Safe version: check for buffer overflow, and prevent it.
    /// The buffer overflow is not signaled, instead it will be revealed later on using [`BIT_closeCStream`].
    ///
    /// Assumption: `bitContainer` has not overflowed
    #[inline]
    pub(crate) unsafe fn flush_bits(&mut self) {
        let nbBytes = (self.bitPos >> 3) as usize;
        debug_assert!(self.bitPos < BitContainerType::BITS);
        debug_assert!(self.ptr <= self.endPtr);
        MEM_writeLEST(self.ptr as *mut core::ffi::c_void, self.bitContainer);
        self.ptr = Ord::min(self.ptr.add(nbBytes), self.endPtr);
        self.bitPos &= 7;
        self.bitContainer >>= nbBytes * 8;
    }

    /// Unsafe version: does not check buffer overflow.
    ///
    /// Assumption: `bitContainer` has not overflowed
    #[inline]
    pub(crate) unsafe fn flush_bits_fast(&mut self) {
        let nbBytes = (self.bitPos >> 3) as usize;
        debug_assert!(self.bitPos < BitContainerType::BITS);
        debug_assert!(self.ptr <= self.endPtr);
        MEM_writeLEST(self.ptr as *mut core::ffi::c_void, self.bitContainer);
        self.ptr = self.ptr.add(nbBytes);
        self.bitPos &= 7;
        self.bitContainer >>= nbBytes * 8;
    }

    /// Close the bit stream.
    ///
    /// # Returns
    ///
    /// - The size of the stream in bytes
    /// - 0 if it could not fit into `dstBuffer`
    #[inline]
    pub(crate) unsafe fn close(mut self) -> usize {
        self.add_bits_fast(1, 1);
        self.flush_bits();
        if self.ptr >= self.endPtr {
            return 0; // overflow detected
        }
        self.ptr
            .offset_from_unsigned(self.startPtr)
            .wrapping_add((self.bitPos > 0) as usize)
    }
}

const BIT_MASK_SIZE: usize = 32;

/// Indexed by a `u8`, so the lookup needs no bounds check. Only the first [`BIT_MASK_SIZE`] entries are used.
static BIT_mask: [core::ffi::c_uint; 256] = {
    let mut mask = [u32::MAX; 256];
    let mut i = 0;
    while i < BIT_MASK_SIZE {
        mask[i] = (1 << i) - 1;
        i += 1;
    }
    mask
};

#[inline(always)]
fn BIT_getLowerBits(bitContainer: BitContainerType, nbBits: u32) -> BitContainerType {
    cfg_select! {
        target_feature = "bmi2" => {
            // With bmi2 the bzhi instruction can be used.
            bitContainer & ((1 << nbBits) - 1)
        }
        _ => {
            // At least on x86_64, the lookup table is faster without bmi2.
            debug_assert!(nbBits < BIT_MASK_SIZE as u32);
            bitContainer & BIT_mask[usize::from(nbBits as u8)] as BitContainerType
        }
    }
}

/// Bitstream decoder
///
/// *  The `new` method stores a chunk of the bitStream is into a local register.
/// *  Local register size is 64-bits on 64-bits systems, 32-bits on 32-bits systems.
/// *  You can then retrieve bitFields stored into the local register, **in reverse order**.
/// *  Local register is explicitly reloaded from memory by the [`reload`] method.
/// *  A reload guarantee a minimum of ((8*sizeof(bitD->bitContainer))-7) bits when its result is [`StreamStatus::Unfinished`].
/// *  Otherwise, it can be less than that, so proceed accordingly.
/// *  Checking if `DStream` has reached its end can be performed with [`BIT_endOfDStream`].
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub(crate) struct BIT_DStream_t<'a> {
    pub(crate) bitContainer: usize,
    pub(crate) bitsConsumed: core::ffi::c_uint,
    pub(crate) ptr: *const core::ffi::c_char,
    pub(crate) start: *const core::ffi::c_char,
    pub(crate) limitPtr: *const core::ffi::c_char,
    pub(crate) _marker: PhantomData<&'a [u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum StreamStatus {
    /// Fully refilled.
    Unfinished = 0,
    /// Still some bits left in the bitstream.
    EndOfBuffer = 1,
    /// Bitstream entirely consumed, bit-exact.
    Completed = 2,
    /// User requested more bits than present in bitstream.
    Overflow = 3,
}

#[inline(always)]
const fn get_middle_bits(
    bitContainer: BitContainerType,
    start: u32,
    nbBits: u32,
) -> BitContainerType {
    const REG_MASK: u32 = BitContainerType::BITS - 1;

    // if start > regMask, the bitstream is corrupted, and the result is undefined.
    debug_assert!(nbBits < BIT_MASK_SIZE as u32);

    #[cfg(feature = "unsafe-performance-experimental")]
    if nbBits >= BIT_MASK_SIZE as u32 {
        unsafe { std::hint::unreachable_unchecked() };
    }

    if cfg!(target_arch = "x86_64") {
        // x86 transform & ((1 << nbBits) - 1) to bzhi instruction, it is better
        // than accessing memory. When bmi2 instruction is not present, we consider
        // such cpus old (pre-Haswell, 2013) and their performance is not of that
        // importance.
        bitContainer >> (start & REG_MASK) & ((1usize << nbBits) - 1)
    } else {
        bitContainer >> (start & REG_MASK) & BIT_mask[nbBits as u8 as usize] as BitContainerType
    }
}

impl<'a> BIT_DStream_t<'a> {
    pub(crate) fn new(srcBuffer: &'a [u8]) -> Result<Self, Error> {
        if srcBuffer.is_empty() {
            return Err(Error::srcSize_wrong);
        }

        const USIZE_BYTES: usize = size_of::<BitContainerType>();

        let start = srcBuffer.as_ptr() as *const core::ffi::c_char;
        let limitPtr = start.wrapping_add(USIZE_BYTES);

        // None when endMark not present
        let bitsConsumed = srcBuffer
            .last()
            .and_then(|v| v.checked_ilog2())
            .map(|v| 8 - v);

        if let Some(chunk) = srcBuffer.last_chunk() {
            Ok(Self {
                bitContainer: usize::from_le_bytes(*chunk),
                bitsConsumed: bitsConsumed.ok_or(Error::GENERIC)?,
                ptr: start
                    .wrapping_add(srcBuffer.len())
                    .wrapping_sub(USIZE_BYTES),
                start,
                limitPtr,
                _marker: PhantomData,
            })
        } else {
            let mut bitContainer = usize::from(srcBuffer[0]);
            if srcBuffer.len() >= 7 {
                bitContainer += usize::from(srcBuffer[6]) << (USIZE_BYTES * 8 - 16);
            }
            if srcBuffer.len() >= 6 {
                bitContainer += usize::from(srcBuffer[5]) << (USIZE_BYTES * 8 - 24);
            }
            if srcBuffer.len() >= 5 {
                bitContainer += usize::from(srcBuffer[4]) << (USIZE_BYTES * 8 - 32);
            }
            if srcBuffer.len() >= 4 {
                bitContainer += usize::from(srcBuffer[3]) << 24;
            }
            if srcBuffer.len() >= 3 {
                bitContainer += usize::from(srcBuffer[2]) << 16;
            }
            if srcBuffer.len() >= 2 {
                bitContainer += usize::from(srcBuffer[1]) << 8;
            }

            Ok(Self {
                bitContainer,
                bitsConsumed: bitsConsumed.ok_or(Error::corruption_detected)?
                    + ((USIZE_BYTES - srcBuffer.len()) * 8) as u32,
                ptr: start,
                start,
                limitPtr,
                _marker: PhantomData,
            })
        }
    }

    /// Provides the next n bits from local register.
    /// The local register is not modified.
    ///
    /// - On 32-bit systems, `maxNbBits==24`.
    /// - On 64-bit systems, `maxNbBits==56`.
    #[inline(always)]
    const fn look_bits(&self, nbBits: u32) -> BitContainerType {
        get_middle_bits(
            self.bitContainer,
            BitContainerType::BITS
                .wrapping_sub(self.bitsConsumed)
                .wrapping_sub(nbBits),
            nbBits,
        )
    }

    /// Like [`look_bits`], but only works when `nbBits >= 1`
    #[inline]
    pub(crate) const fn look_bits_fast(&self, nbBits: u32) -> BitContainerType {
        // quickcheck hits this
        // debug_assert!(nbBits > 1);

        const MASK: u32 = (BitContainerType::BITS as usize - 1) as u32;
        self.bitContainer << (self.bitsConsumed & MASK)
            >> (MASK.wrapping_add(1).wrapping_sub(nbBits) & MASK)
    }

    #[inline(always)]
    pub(crate) const fn skip_bits(&mut self, nbBits: u32) {
        self.bitsConsumed += nbBits;
    }

    /// Read (consume) next n bits from local register and update.
    ///
    /// Pay attention to not read more than nbBits contained into local register.
    #[inline(always)]
    pub(crate) const fn read_bits(&mut self, nbBits: core::ffi::c_uint) -> BitContainerType {
        let value = self.look_bits(nbBits);
        self.skip_bits(nbBits);
        value
    }

    /// Like [`read_bits`], but only works when `nbBits >= 1`
    #[inline]
    pub(crate) const fn read_bits_fast(&mut self, nbBits: core::ffi::c_uint) -> BitContainerType {
        // quickcheck hits this
        // debug_assert!(nbBits > 1);

        let value = self.look_bits_fast(nbBits);
        self.skip_bits(nbBits);
        value
    }

    ///  Simple variant of [`reload`] with two conditions:
    ///
    ///  1. bitstream is valid : bitsConsumed <= sizeof(bitD->bitContainer)*8
    ///  2. look window is valid after shifted down : bitD->ptr >= bitD->start
    #[inline]
    unsafe fn reload_internal(&mut self) {
        // quickcheck hits this
        // debug_assert!(self.bitsConsumed as usize >= 8 * size_of::<usize>());
        self.ptr = unsafe { self.ptr.sub(self.bitsConsumed as usize / 8) };
        debug_assert!(self.ptr >= self.start);
        self.bitsConsumed &= 7;
        self.bitContainer = unsafe { core::ptr::read_unaligned(self.ptr as *const usize) }.to_le();
    }

    #[inline]
    pub(crate) fn reload_fast(&mut self) -> StreamStatus {
        if self.ptr < self.limitPtr {
            StreamStatus::Overflow
        } else {
            unsafe { self.reload_internal() };
            StreamStatus::Unfinished
        }
    }

    ///  Refill from the buffer
    ///
    ///  ## Returns
    ///
    /// status of the internal register. when `status == StreamStatus::Unfinished`,
    /// the internal register is filled with at least 25 or 57 bits.
    #[inline(always)]
    pub(crate) fn reload(&mut self) -> StreamStatus {
        if unlikely(self.bitsConsumed > BitContainerType::BITS) {
            static zeroFilled: BitContainerType = 0;
            self.ptr = &zeroFilled as *const BitContainerType as *const core::ffi::c_char;

            return StreamStatus::Overflow;
        }

        if self.ptr >= self.limitPtr {
            unsafe { self.reload_internal() };

            return StreamStatus::Unfinished;
        }

        if self.ptr == self.start {
            return if self.bitsConsumed < BitContainerType::BITS {
                StreamStatus::EndOfBuffer
            } else {
                StreamStatus::Completed
            };
        }

        let mut nbBytes = self.bitsConsumed as usize / 8;
        let result = if unsafe { self.ptr.sub(nbBytes) } < self.start {
            nbBytes = unsafe { self.ptr.offset_from_unsigned(self.start) };

            StreamStatus::EndOfBuffer
        } else {
            StreamStatus::Unfinished
        };

        self.ptr = unsafe { self.ptr.sub(nbBytes) };
        self.bitsConsumed = self.bitsConsumed.wrapping_sub((nbBytes * 8) as u32);
        self.bitContainer = unsafe { core::ptr::read_unaligned(self.ptr as *const usize) }.to_le();

        result
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ptr == self.start && self.bitsConsumed == BitContainerType::BITS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_empty_buffer() {
        let result = BIT_DStream_t::new(&[]);
        assert!(result.is_err(), "Should fail on empty input");
    }

    #[test]
    fn test_new_ilog2_undefined() {
        let buffer = vec![0b00000000];
        let result = BIT_DStream_t::new(&buffer);
        assert!(result.is_err(), "Should fail when ilog2 is undefined");
    }

    const END_MARK: u8 = 0b00001000;
    const END_MARK_LOG: u32 = END_MARK.ilog2();

    #[test]
    fn test_new_success() {
        let buffer = [0b10101010, 0b11110000, END_MARK];
        let stream = BIT_DStream_t::new(&buffer).expect("BIT_DStream_t::new should succeed");
        assert!(stream.ptr >= stream.start);
        assert_ne!(stream.bitContainer, 0);
    }

    #[test]
    fn test_look_and_read_bits() {
        let buffer = [0b10101010, 0b11001100, END_MARK];
        let mut stream = BIT_DStream_t::new(&buffer).unwrap();

        let consumed = stream.bitsConsumed;

        let bits = stream.look_bits(12 + END_MARK_LOG);
        assert_eq!(bits, 0b1100_1100_1010);

        let read = stream.read_bits(12 + END_MARK_LOG);
        assert_eq!(bits, read);

        assert_eq!(stream.bitsConsumed - consumed, 12 + END_MARK_LOG);
    }

    #[test]
    fn test_look_and_read_bits_fast() {
        let buffer = [0xFF, 0xAA, END_MARK];
        let mut stream = BIT_DStream_t::new(&buffer).unwrap();

        let consumed = stream.bitsConsumed;

        let bits = stream.look_bits_fast(12 + END_MARK_LOG);
        assert_eq!(bits, 0xAAF);

        let read = stream.read_bits_fast(20 + END_MARK_LOG);
        assert_eq!(read, 0xAAFF0);

        assert_eq!(stream.bitsConsumed - consumed, 20 + END_MARK_LOG);
    }

    #[test]
    fn test_end_mark() {
        let end_mark = 0b0001_0000u8;
        let end_mark_log = end_mark.ilog2();

        let buffer = [0b10101010, 0b11001100, end_mark];
        let mut stream = BIT_DStream_t::new(&buffer).unwrap();

        let consumed = stream.bitsConsumed;

        let bits = stream.look_bits(12 + end_mark_log);
        assert_eq!(bits, 0b1100_1100_1010);

        let read = stream.read_bits(12 + end_mark_log);
        assert_eq!(bits, read);

        assert_eq!(stream.bitsConsumed - consumed, 12 + end_mark_log);
    }

    #[test]
    fn test_reload_end_of_buffer() {
        let buffer = [0b00001111, END_MARK];
        let mut stream = BIT_DStream_t::new(&buffer).unwrap();

        assert!(
            matches!(
                stream.reload(),
                StreamStatus::EndOfBuffer | StreamStatus::Completed
            ),
            "Should be at end of buffer or complete"
        );
    }

    #[test]
    fn test_reload_overflow() {
        let mut stream = BIT_DStream_t {
            bitContainer: 0,
            bitsConsumed: BitContainerType::BITS + 1,
            ptr: core::ptr::null(),
            start: core::ptr::null(),
            limitPtr: core::ptr::null(),
            _marker: PhantomData,
        };

        assert_eq!(stream.reload(), StreamStatus::Overflow);
    }
}
