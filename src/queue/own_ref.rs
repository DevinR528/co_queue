use std::borrow::{Borrow, BorrowMut};
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::Condvar;

pub use crate::{MapMootexGuard, Mootex, MootexGuard};

/// Panics if the pointer is not properly unaligned.
#[inline]
fn ensure_aligned<T>(raw: *const T) {
    assert_eq!(raw as usize & low_bits::<T>(), 0, "unaligned pointer");
}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
#[inline]
fn low_bits<T>() -> usize {
    (1 << mem::align_of::<T>().trailing_zeros()) - 1
}

/// Given a tagged pointer `data`, returns the same pointer, but tagged with `tag`.
///
/// `tag` is truncated to fit into the unused bits of the pointer to `T`.
#[inline]
fn data_with_tag<T>(data: usize, tag: usize) -> usize {
    (data & !low_bits::<T>()) | (tag & low_bits::<T>())
}

/// Decomposes a tagged pointer `data` into the pointer and the tag.
#[inline]
fn decompose_data<T>(data: usize) -> *mut T {
    (data & !low_bits::<T>()) as *mut T
}

pub struct Owned<T> {
    /// Exclusive pointer to data.
    data: usize,
    /// This owns `T` and acts like a `Box`
    /// this allows us to go between Shared and Owned pointer
    /// to allow this.
    ///
    ///  # Example
    /// ```rust
    /// fn push(&self, val: T) {
    ///     let mut node = Node::new(val);
    ///     
    ///     loop { unsafe {
    ///         if let Err(val_back) = self._push(node) {
    ///             node = val_back;
    ///         }
    ///     }}
    /// }
    /// ```
    _mk: PhantomData<Box<T>>,
}

impl<T> Owned<T> {
    pub fn new(val: T) -> Owned<T> {
        let data = Box::into_raw(Box::new(val));
        Self::from_raw(data)
    }
    pub fn from_raw(data: *mut T) -> Owned<T> {
        ensure_aligned(data);
        Owned {
            data: data as usize,
            _mk: PhantomData,
        }
    }
    pub fn from_usize(data: usize) -> Owned<T> {
        debug_assert!(data != 0, "converting zero into `Owned`");
        Owned {
            data,
            _mk: PhantomData,
        }
    }
    pub fn null() -> Self {
        Self {
            data: 0,
            _mk: PhantomData,
        }
    }
    pub fn as_raw(&self) -> *const T {
        decompose_data::<T>(self.data)
    }
    pub fn is_null(&self) -> bool {
        self.as_raw().is_null()
    }
    // pub fn into_shared<'s>(self) -> Shared<'s, T> {
    //     println!("into_shared {:?}", self);
    //     Shared::from_raw(self.data as *const T)
    // }
}
impl<T: fmt::Debug> Owned<T> {
    pub fn into_shared<'s>(self) -> Shared<'s, T> {
        println!("into_shared {:?}", self.data);
        Shared::from_raw(self.data as *const T)
    }
}

impl<T: fmt::Debug> fmt::Debug for Owned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dbg = unsafe {
            if self.is_null() {
                &"null" as &dyn fmt::Debug
            } else {
                &*self.as_raw() as &dyn fmt::Debug
            }
        };
        f.debug_struct("Owned").field("data", dbg).finish()
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        unsafe { drop(Box::from_raw(self.data as *mut T)) }
    }
}

impl<T> From<T> for Owned<T> {
    fn from(val: T) -> Self {
        Owned::new(val)
    }
}

impl<T> Deref for Owned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.as_raw() }
    }
}

impl<T> DerefMut for Owned<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *(self.data as *mut _) }
    }
}

impl<T> Borrow<T> for Owned<T> {
    fn borrow(&self) -> &T {
        &**self
    }
}

impl<T> BorrowMut<T> for Owned<T> {
    fn borrow_mut(&mut self) -> &mut T {
        &mut **self
    }
}

impl<T> AsRef<T> for Owned<T> {
    fn as_ref(&self) -> &T {
        &**self
    }
}

impl<T> AsMut<T> for Owned<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut **self
    }
}

pub struct Shared<'s, T> {
    /// TODO NonNull??
    /// This is not for mutation this is to pass ref like pointers
    /// to places where eventually owned are needed and is proved
    /// exclusive thus safe.
    data: usize,
    /// the lifetime and type variance are kept by holding the
    /// equivalent type information in `_mk`.
    _mk: PhantomData<(&'s (), *const T)>,
}

impl<'s, T> Shared<'s, T> {
    
    pub fn null() -> Self {
        Self {
            data: 0,
            _mk: PhantomData,
        }
    }
    pub fn as_raw(&self) -> *const T {
        decompose_data::<T>(self.data)
    }
    pub fn is_null(&self) -> bool {
        self.as_raw().is_null()
    }
}
impl<'s, T: fmt::Debug> Shared<'s, T> {
    pub fn from_raw(data: *const T) -> Shared<'s, T> {
        ensure_aligned(data);
        unsafe { println!("from raw {:?}", *data) };
        let s = Shared {
            data: data as usize,
            _mk: PhantomData,
        };
        println!("from raw shared {:?}", s);
        s
    }
    pub fn into_owned(self) -> Owned<T> {
        unsafe { println!("into owned {:?}", *self.as_raw()) };
        Owned::from_usize(self.data)
    }
}

impl<'s, T: fmt::Debug> fmt::Debug for Shared<'s, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dbg = unsafe {
            if self.is_null() {
                &"null" as &dyn fmt::Debug
            } else {
                &*self.as_raw() as &dyn fmt::Debug
            }
        };
        f.debug_struct("Shared").field("data", dbg).finish()
    }
}

impl<'s, T: fmt::Debug> Clone for Shared<'s, T> {
    fn clone(&self) -> Self {
        println!("CLONE OF {:?}", self);
        let s = Self {
            data: self.data,
            _mk: PhantomData,
        };
        println!("COPY OF {:?}", s);
        s
    }
}

impl<'s, T: fmt::Debug> Copy for Shared<'s, T> {}

impl<'s, T> Deref for Shared<'s, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.as_raw() }
    }
}

impl<'s, T> DerefMut for Shared<'s, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *(self.data as *mut _) }
    }
}
