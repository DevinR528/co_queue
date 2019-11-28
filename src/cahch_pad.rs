use core::fmt;
use core::ops::{Deref, DerefMut};

/// Pads and aligns a value to the length of a cache line.
///
/// In concurrent programming, sometimes it is desirable to make sure commonly accessed pieces of
/// data are not placed into the same cache line. Updating an atomic value invalides the whole
/// cache line it belongs to, which makes the next access to the same cache line slower for other
/// CPU cores. Use `CachePad` to ensure updating one piece of data doesn't invalidate other
/// cached data.
///
/// # Size and alignment
///
/// Cache lines are assumed to be N bytes long, depending on the architecture:
///
/// * On x86-64, N = 128.
/// * On all others, N = 64.
///
/// Note that N is just a reasonable guess and is not guaranteed to match the actual cache line
/// length of the machine the program is running on. On modern Intel architectures, spatial
/// prefetcher is pulling pairs of 64-byte cache lines at a time, so we pessimistically assume that
/// cache lines are 128 bytes long.
///
/// The size of `CachePad<T>` is the smallest multiple of N bytes large enough to accommodate
/// a value of type `T`.
///
/// The alignment of `CachePad<T>` is the maximum of N bytes and the alignment of `T`.
#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]
#[cfg_attr(target_arch = "x86_64", repr(align(128)))]
#[cfg_attr(not(target_arch = "x86_64"), repr(align(64)))]
pub struct CachePad<T> {
    value: T,
}

unsafe impl<T: Send> Send for CachePad<T> {}
unsafe impl<T: Sync> Sync for CachePad<T> {}

impl<T> CachePad<T> {

    pub fn new(t: T) -> CachePad<T> {
        CachePad::<T> { value: t }
    }

    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for CachePad<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePad<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for CachePad<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CachePad")
            .field("value", &self.value)
            .finish()
    }
}

impl<T> From<T> for CachePad<T> {
    fn from(t: T) -> Self {
        CachePad::new(t)
    }
}
