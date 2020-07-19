//! "Pointer"s into a syntax tree.
//!
//! These pointers are "trace"s down a syntax tree, e.g. `[4, 2, 0]` says to
//! take the 4th child, the 2nd child, and then the 0th child. To reduce the
//! size and allocation requirements of syntax pointers, they perform a "small
//! vector" optimization, storing the trace inline if it takes 15 or fewer
//! bytes to represent. Additionally, a custom variable-length encoding is
//! used to store pointers compactly while still supporting large trees.
//!
//! The actual trace encoding is as follows:
//!
//! - For child indices < 240 (0xF0),
//!       store the index as a byte, as-is.
//! - For child indices >= 240 (0xF0),
//!       store the index as two bytes, 0xF_ 0x__.
//! - Indices > 4095 (0x0FFF) are unrepresentable.

use smallvec::SmallVec;
use std::{convert::TryInto, fmt, mem, slice};

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Rowan currently only works on 32 or 64 bit targets");

/// A pointer trace in some syntax tree to a specific node.
#[repr(C)]
#[cfg_attr(target_pointer_width = "32", repr(align(4)))]
#[cfg_attr(target_pointer_width = "64", repr(align(8)))]
pub struct SyntaxPtr {
    data: SyntaxPtrData,
    len: u8,
}

#[repr(C, packed)]
union SyntaxPtrData {
    inline: [u8; 15],
    outline: *mut u8,
}

unsafe impl Send for SyntaxPtrData {}
unsafe impl Sync for SyntaxPtrData {}

#[forbid(const_err)]
const _ASSERT_SYNTAX_PTR_IS_ALIGNED_PROPERLY: [(); std::mem::align_of::<SyntaxPtr>()] =
    [(); std::mem::align_of::<*const u8>()];

impl fmt::Debug for SyntaxPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxPtr").field("data", &self.data()).finish()
    }
}

impl Default for SyntaxPtr {
    fn default() -> Self {
        SyntaxPtr { data: SyntaxPtrData { inline: [0; 15] }, len: 0 }
    }
}

impl Clone for SyntaxPtr {
    fn clone(&self) -> Self {
        let data = if self.is_inline() {
            SyntaxPtrData { inline: unsafe { self.data.inline } }
        } else {
            let this = unsafe { mem::ManuallyDrop::new(Box::from_raw(self.outline())) };
            SyntaxPtrData { outline: Box::into_raw(Box::clone(&this)) as *mut u8 }
        };
        SyntaxPtr { data, len: self.len }
    }
}

impl SyntaxPtr {
    pub(crate) fn try_new(indices: impl Iterator<Item = u16> + ExactSizeIterator) -> Option<Self> {
        let mut vec = SmallVec::<[u8; 15]>::with_capacity(indices.len());

        for index in indices {
            if index < 0xF0 {
                vec.push(index as u8)
            } else if index <= 0x0FFF {
                vec.push((index >> 8) as u8 | 0xF0);
                vec.push(index as u8);
            } else {
                return None;
            }
        }

        let len = match vec.len().try_into() {
            Ok(len) => len,
            Err(_) => return None,
        };

        let data = match vec.into_inner() {
            Ok(data) => SyntaxPtrData { inline: data },
            Err(vec) => {
                debug_assert!(len > 15);
                SyntaxPtrData { outline: Box::into_raw(vec.into_boxed_slice()) as *mut u8 }
            }
        };

        Some(SyntaxPtr { data, len })
    }
}

impl Drop for SyntaxPtr {
    fn drop(&mut self) {
        if !self.is_inline() {
            unsafe { Box::from_raw(self.outline()) };
        }
    }
}

impl SyntaxPtr {
    fn is_inline(&self) -> bool {
        self.len < 16
    }

    unsafe fn inline(&self) -> &[u8] {
        &self.data.inline.get_unchecked(..self.len as _)
    }

    unsafe fn outline(&self) -> *mut [u8] {
        // Tell the compiler it can do an aligned read
        let ptr = *&self.data.outline;
        slice::from_raw_parts_mut(ptr, self.len as _)
    }

    fn data(&self) -> &[u8] {
        if self.is_inline() {
            unsafe { self.inline() }
        } else {
            unsafe { &*self.outline() }
        }
    }

    pub(crate) fn trace(&self) -> SyntaxTrace<'_> {
        unsafe { SyntaxTrace::new(self.data()) }
    }
}

pub(crate) struct SyntaxTrace<'a> {
    data: slice::Iter<'a, u8>,
}

impl<'a> SyntaxTrace<'a> {
    unsafe fn new(data: &'a [u8]) -> Self {
        SyntaxTrace { data: data.iter() }
    }
}

impl Iterator for SyntaxTrace<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        let mut n = *self.data.next()? as u16;
        if n < 0xF0 {
            return Some(n);
        }

        n <<= 8;
        n |= *self.data.next().unwrap() as u16;
        Some(n & 0x0FFF)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.data.len() / 2, Some(self.data.len()))
    }
}
