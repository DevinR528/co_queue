use std::borrow::{Borrow, BorrowMut};
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::Condvar;

pub use crate::{MapMootexGuard, Mootex, MootexGuard};

pub struct Owned<T> {
    /// Exclusive pointer to data.
    data: *const T,
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
        Self {
            data,
            _mk: PhantomData,
        }
    }
    pub fn from_raw(data: *mut T) -> Owned<T> {
        Owned {
            data,
            _mk: PhantomData,
        }
    }
    pub fn into_shared<'s>(self) -> Shared<'s, T> {
        Shared::from_raw(self.data as *const T)
    }
}

impl<T: fmt::Debug> fmt::Debug for Owned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dbg = unsafe {
            if self.data.is_null() {
                &"null" as &dyn fmt::Debug
            } else {
                &*self.data as &dyn fmt::Debug
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
        unsafe { &*self.data }
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
    data: *const T,
    /// the lifetime and type variance are kept by holding the
    /// equivalent type information in `_mk`.
    _mk: PhantomData<(&'s (), *const T)>,
}

impl<'s, T> Shared<'s, T> {
    pub fn null() -> Shared<'s, T> {
        Self {
            data: ptr::null(),
            _mk: PhantomData,
        }
    }
    pub fn from_raw(data: *const T) -> Shared<'s, T> {
        Shared {
            data,
            _mk: PhantomData,
        }
    }
    pub fn is_null(&self) -> bool {
        self.data.is_null()
    }
    pub fn as_ptr(&self) -> *const T {
        self.data
    }
    pub fn into_owned(self) -> Owned<T> {
        debug_assert!(!self.is_null());
        Owned::from_raw(self.data as *mut T)
    }
}

impl<'s, T: fmt::Debug> fmt::Debug for Shared<'s, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dbg = unsafe {
            if self.data.is_null() {
                &"null" as &dyn fmt::Debug
            } else {
                &*self.data as &dyn fmt::Debug
            }
        };
        f.debug_struct("Shared").field("data", dbg).finish()
    }
}

impl<'s, T> Clone for Shared<'s, T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data,
            _mk: PhantomData,
        }
    }
}

impl<'s, T> Copy for Shared<'s, T> {}

impl<'s, T> Deref for Shared<'s, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.data }
    }
}

impl<'s, T> DerefMut for Shared<'s, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *(self.data as *mut _) }
    }
}
