use std::borrow::{Borrow, BorrowMut};
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{
    self, AtomicPtr, AtomicUsize,
    Ordering::{self, *},
};
use std::sync::Condvar;

pub use crate::{MapMootexGuard, Mootex, MootexGuard};

/// A trait for either `Owned` or `Shared` pointers.
pub trait Pointer<T: fmt::Debug> {
    /// Returns the machine representation of the pointer.
    fn into_usize(self) -> usize;

    /// Returns a new pointer pointing to the tagged pointer `data`.
    unsafe fn from_usize(data: usize) -> Self;
}

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
pub struct Atomic<T> {
    data: AtomicUsize,
    _mk: PhantomData<*mut T>,
}

unsafe impl<T: Send + Sync> Send for Atomic<T> {}
unsafe impl<T: Send + Sync> Sync for Atomic<T> {}

impl<T: fmt::Debug> Atomic<T> {
    pub fn new(val: T) -> Atomic<T> {
        Self::from(Owned::new(val))
    }

    pub fn from_usize(data: usize) -> Atomic<T> {
        debug_assert!(data != 0, "converting zero into `Atomic`");
        Atomic {
            data: AtomicUsize::new(data),
            _mk: PhantomData,
        }
    }
    pub fn null() -> Self {
        Self {
            data: AtomicUsize::new(0),
            _mk: PhantomData,
        }
    }

    pub fn load(&self, ord: Ordering) -> Shared<'_, T> {
        unsafe { Shared::from_usize(self.data.load(ord)) }
    }

    pub fn store<'g, P: Pointer<T>>(&self, new: P, ord: Ordering) {
        self.data.store(new.into_usize(), ord);
    }

    pub fn compare_set<'g, P>(
        &self,
        curr: Shared<T>,
        new: P,
        ord: Ordering,
    ) -> Result<Shared<'g, T>, Shared<'g, T>>
    where
        P: Pointer<T>,
    {
        let new = new.into_usize(); // next
        let curr = curr.into_usize(); // head
        unsafe {
            self.data
                .compare_exchange(curr, new, ord, Ordering::Relaxed)
                .map(|_| Shared::from_usize(curr)) // head
                .map_err(|_| Shared::from_usize(new)) // next
        }
    }

    pub fn compare_set_weak<'g, P>(
        &self,
        curr: Shared<T>,
        new: P,
        ord: Ordering,
    ) -> Result<Shared<'g, T>, Shared<'g, T>>
    where
        P: Pointer<T>,
    {
        let new = new.into_usize();
        let curr = curr.into_usize();
        unsafe {
            self.data
                .compare_exchange_weak(curr, new, ord, Ordering::Relaxed)
                .map(|_| Shared::from_usize(curr))
                .map_err(|_| Shared::from_usize(new))
        }
    }

    pub fn fetch_add<'g>(&self, val: usize, ord: Ordering) -> Shared<'g, T> {
        unsafe { Shared::from_usize(self.data.fetch_add(val, ord)) }
    }

    pub unsafe fn into_owned(self) -> Owned<T> {
        Owned::from_usize(self.data.into_inner())
    }
}

impl<T: fmt::Debug> fmt::Debug for Atomic<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dbg = unsafe {
            let shared: Shared<'_, T> = Shared::from_usize(self.data.load(Relaxed));
            if shared.is_null() {
                &"null" as &dyn fmt::Debug
            } else {
                &*shared.as_raw() as &dyn fmt::Debug
            }
        };
        f.debug_struct("Atomic").field("data", dbg).finish()
    }
}
impl<'g, T: fmt::Debug> From<Shared<'g, T>> for Atomic<T> {
    fn from(ptr: Shared<'g, T>) -> Self {
        Self::from_usize(ptr.data)
    }
}
impl<T: fmt::Debug> From<Owned<T>> for Atomic<T> {
    fn from(owned: Owned<T>) -> Self {
        let data = owned.data;
        mem::forget(owned);
        Self::from_usize(data)
    }
}
impl<T: fmt::Debug> From<*const T> for Atomic<T> {
    fn from(raw: *const T) -> Self {
        Self::from_usize(raw as usize)
    }
}

pub struct Owned<T> {
    /// Exclusive pointer to data.
    data: usize,
    /// This owns `T` and acts like a `Box`
    /// this allows us to go between Shared and Owned pointer
    /// to allow this.
    ///
    ///  # Example
    /// ```ignore
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
        Self::from_raw(Box::into_raw(Box::new(val)))
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
}

impl<T: fmt::Debug> Owned<T> {
    pub fn into_shared<'s>(self) -> Shared<'s, T> {
        unsafe { Shared::from_usize(self.into_usize()) }
    }
}

impl<T: fmt::Debug> Pointer<T> for Owned<T> {
    #[inline]
    fn into_usize(self) -> usize {
        let data = self.data;
        mem::forget(self);
        data
    }

    #[inline]
    unsafe fn from_usize(data: usize) -> Self {
        debug_assert!(data != 0, "converting zero into `Owned`");
        Owned {
            data,
            _mk: PhantomData,
        }
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
        let raw = decompose_data::<T>(self.data);
        unsafe { drop(Box::from_raw(raw)) }
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

impl<T: Clone> Clone for Owned<T> {
    fn clone(&self) -> Self {
        Owned::new((**self).clone())
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
    pub fn as_raw_mut(&self) -> *mut T {
        decompose_data::<T>(self.data) as *mut T
    }
    pub fn is_null(&self) -> bool {
        self.as_raw().is_null()
    }
}
impl<'s, T: fmt::Debug> Shared<'s, T> {
    pub fn from_raw(data: *const T) -> Shared<'s, T> {
        ensure_aligned(data);
        Shared {
            data: data as usize,
            _mk: PhantomData,
        }
    }
    pub fn into_owned(self) -> Owned<T> {
        Owned::from_usize(self.data)
    }
}
impl<'s, T: fmt::Debug> Pointer<T> for Shared<'s, T> {
    unsafe fn from_usize(data: usize) -> Self {
        Shared {
            data,
            _mk: PhantomData,
        }
    }
    fn into_usize(self) -> usize {
        self.data
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
        Self {
            data: self.data,
            _mk: PhantomData,
        }
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
