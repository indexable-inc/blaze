use core::ops::{Bound, Range};
use core::ptr::NonNull;

/// Bounds checks like a slice, but without the aliasing requirements.
#[derive(Debug)]
pub struct Reader<'a> {
    ptr: Option<NonNull<u8>>,
    end: *const u8,
    _marker: core::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> Reader<'a> {
    /// # Safety
    ///
    /// - `ptr` must point to `len` readable bytes
    /// - `ptr` may be NULL only if `len == 0`
    pub unsafe fn from_raw_parts(ptr: *const u8, len: usize) -> Self {
        let ptr = NonNull::new(ptr.cast_mut());

        if ptr.is_none() {
            assert_eq!(len, 0);
        }

        Self {
            ptr,
            end: match ptr {
                None => core::ptr::null(),
                Some(ptr) => unsafe { ptr.as_ptr().add(len) },
            },
            _marker: core::marker::PhantomData,
        }
    }

    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            ptr: NonNull::new(slice.as_ptr().cast_mut()),
            end: unsafe { slice.as_ptr().add(slice.len()) },
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self.ptr {
            None => 0,
            Some(ptr) => unsafe { self.end.offset_from_unsigned(ptr.as_ptr()) },
        }
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        self.ptr.is_none()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        match self.ptr {
            None => true,
            Some(ptr) => ptr.as_ptr().cast_const() == self.end,
        }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        match self.ptr {
            None => core::ptr::null_mut(),
            Some(ptr) => ptr.as_ptr(),
        }
    }

    #[inline]
    pub fn as_ptr_range(&self) -> Range<*const u8> {
        self.as_ptr()..self.as_ptr().wrapping_add(self.len())
    }

    //    #[inline]
    //    pub fn as_ptr_range(&mut self) -> core::ops::Range<*const u8> {
    //        match self.ptr {
    //            None => core::ptr::null_mut()..core::ptr::null(),
    //            Some(ptr) => ptr.as_ptr().cast_const()..self.end,
    //        }
    //    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self.ptr {
            None => &[],
            Some(ptr) => {
                let ptr = ptr.as_ptr().cast_const();
                unsafe { core::slice::from_raw_parts(ptr, self.end.offset_from_unsigned(ptr)) }
            }
        }
    }

    pub fn subslice<R: core::ops::RangeBounds<usize>>(&self, range: R) -> Self {
        let Some(ptr) = self.ptr else {
            match (range.start_bound(), range.end_bound()) {
                (Bound::Unbounded, Bound::Unbounded)
                | (
                    Bound::Included(&0),
                    Bound::Included(&0) | Bound::Excluded(&1) | Bound::Unbounded,
                ) => {
                    return Self {
                        ptr: self.ptr,
                        end: self.end,
                        _marker: self._marker,
                    };
                }
                _ => panic!("out of bounds"),
            }
        };

        let new_ptr = match range.start_bound() {
            Bound::Included(&count) => ptr.as_ptr().wrapping_add(count),
            Bound::Excluded(_) => unreachable!("I think?"),
            Bound::Unbounded => ptr.as_ptr(),
        };

        if new_ptr.cast_const() > self.end {
            panic!("out of bounds");
        }

        let new_end = match range.end_bound() {
            Bound::Included(&count) => ptr.as_ptr().wrapping_add(count + 1),
            Bound::Excluded(&count) => ptr.as_ptr().wrapping_add(count),
            Bound::Unbounded => self.end,
        };

        if new_end > self.end {
            panic!("out of bounds");
        }

        Self {
            ptr: NonNull::new(new_ptr),
            end: new_end,
            _marker: core::marker::PhantomData,
        }
    }
}
