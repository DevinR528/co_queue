use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};

mod deadlock;
mod raw_mutex;
use raw_mutex::RawMutex;

pub unsafe trait Mutexable {
    /// this is passed back and forth each time a lock is held and
    /// released
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self;

    /// acquires this mutex blocking current thread until able.
    fn lock(&self);
    /// tries to lock mutex is not false is returned.
    fn try_lock(&self) -> bool;
    /// releases the mutex, other threads are able to acquire it.
    fn unlock(&self);
}

#[must_use = "if unused the Mutex will immediately unlock"]
#[derive(Debug)]
pub struct MapMutexGuard<'g, M: Mutexable, T: ?Sized> {
    raw: &'g M,
    data: *mut T,
    _mk: PhantomData<&'g mut T>,
}

unsafe impl<'g, M: Mutexable + Sync + 'g, T: ?Sized + Sync + 'g> Sync for MapMutexGuard<'g, M, T> {}

unsafe impl<'g, M: Mutexable + 'g, T: ?Sized + 'g> Send for MapMutexGuard<'g, M, T> {}

impl<'a, R: Mutexable + 'a, T: ?Sized + 'a> MapMutexGuard<'a, R, T> {
    /// Makes a new `MappedMutexGuard` for a component of the locked data.
    ///
    /// This operation cannot fail as the `MappedMutexGuard` passed
    /// in already locked the mutex.
    ///
    /// This is an associated function that needs to be
    /// used as `MappedMutexGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn map<U: ?Sized, F>(s: Self, f: F) -> MapMutexGuard<'a, R, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let raw = s.raw;
        let data = f(unsafe { &mut *s.data });
        mem::forget(s);
        MapMutexGuard {
            raw,
            data,
            _mk: PhantomData,
        }
    }
}

pub struct MutexGuard<'g, M: Mutexable, T: ?Sized> {
    mutex: &'g SmallMutex<M, T>,
    _mk: PhantomData<(&'g mut T, M)>,
}

unsafe impl<'g, M: Mutexable + Sync + 'g, T: ?Sized + Sync + 'g> Sync for MutexGuard<'g, M, T> {}

impl<'g, M: Mutexable + 'g, T: ?Sized + 'g> Drop for MutexGuard<'g, M, T> {
    fn drop(&mut self) {
        self.mutex.raw.unlock()
    }
}

impl<'g, M: Mutexable + 'g, T: ?Sized + 'g> Deref for MutexGuard<'g, M, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'g, M: Mutexable + 'g, T: ?Sized + 'g> DerefMut for MutexGuard<'g, M, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'g, M: Mutexable + 'g, T: fmt::Debug + ?Sized + 'g> fmt::Debug for MutexGuard<'g, M, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'g, M: Mutexable + 'g, T: ?Sized + 'g> MutexGuard<'g, M, T> {
    pub fn mutex(s: &Self) -> &'g SmallMutex<M, T> {
        s.mutex
    }

    pub fn map<U: ?Sized, F>(s: Self, f: F) -> MapMutexGuard<'g, M, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let raw = &s.mutex.raw;
        let data = f(unsafe { &mut *s.mutex.data.get() });

        mem::forget(s);
        MapMutexGuard {
            raw,
            data,
            _mk: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct SmallMutex<M: Mutexable, T: ?Sized> {
    raw: M,
    data: UnsafeCell<T>,
}

impl<M: Mutexable, T: Default> Default for SmallMutex<M, T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

unsafe impl<R: Mutexable + Send, T: ?Sized + Send> Send for SmallMutex<R, T> {}
unsafe impl<R: Mutexable + Sync, T: ?Sized + Send> Sync for SmallMutex<R, T> {}

impl<M: Mutexable, T> SmallMutex<M, T> {
    #[inline]
    pub fn new(val: T) -> SmallMutex<M, T> {
        SmallMutex {
            raw: M::INIT,
            data: UnsafeCell::from(val),
        }
    }
    #[inline]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<M: Mutexable, T: ?Sized> SmallMutex<M, T> {
    #[inline]
    unsafe fn guard(&self) -> MutexGuard<'_, M, T> {
        MutexGuard {
            mutex: self,
            _mk: PhantomData,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, M, T> {
        self.raw.lock();
        unsafe { self.guard() }
    }

    pub fn try_lock(&self) -> bool {
        self.raw.try_lock()
    }

    pub fn force_unlock(&self) {
        self.raw.unlock()
    }
}

pub type Mootex<T> = SmallMutex<RawMutex, T>;
pub type MootexGuard<'a, T> = MutexGuard<'a, RawMutex, T>;
pub type MapMootexGuard<'a, T> = MapMutexGuard<'a, RawMutex, T>;
