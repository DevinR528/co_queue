use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::Condvar;
use std::sync::Once;

use super::num_threads;
pub use crate::{MapMootexGuard, Mootex, MootexGuard};

pub struct PtrGuard<T> {
    guard: AtomicPtr<T>,
    cap: usize,
    len: AtomicUsize,
    lock: Mootex<()>,
}

impl<T: fmt::Debug> fmt::Debug for PtrGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut v = Vec::default();
        unsafe {
            let ptr = self.guard.load(SeqCst);
            for x in 0..self.len() {
                v.push(ptr::read(ptr.add(x)))
            }
        }

        f.debug_struct("PtrGuard")
            .field("cap", &self.cap)
            .field("len", &self.len)
            .field("data", &v)
            .finish()
    }
}

impl<T> PtrGuard<T> {
    pub fn new() -> PtrGuard<T> {
        let cap = num_threads();
        let mut data = Vec::with_capacity(cap);
        let guard = AtomicPtr::new(data.as_mut_ptr());
        mem::forget(data);
        Self {
            guard,
            cap,
            len: AtomicUsize::new(0),
            lock: Mootex::default(),
        }
    }
    pub fn with_threads(threads: usize) -> PtrGuard<T> {
        let mut data = Vec::with_capacity(threads);
        let guard = AtomicPtr::new(data.as_mut_ptr());
        mem::forget(data);
        Self {
            guard,
            cap: threads,
            len: AtomicUsize::new(0),
            lock: Mootex::default(),
        }
    }
    pub fn try_lock(&self) -> bool {
        self.lock.try_lock()
    }
    pub fn lock(&self) -> MootexGuard<()> {
        self.lock.lock()
    }
    pub fn unlock(&self) {
        self.lock.force_unlock()
    }
    pub fn len(&self) -> usize {
        self.len.load(Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len.load(Relaxed) == 0
    }
    pub fn push(&self, node: T) -> Result<(), T> {
        let len = self.len.load(Acquire);
        if len == self.cap {
            Err(node)
        } else {
            let guard = self.guard.load(SeqCst);
            unsafe { ptr::write(guard.add(len), node) };
            self.len.compare_and_swap(len, len + 1, Release);
            Ok(())
        }
    }
    pub fn pop(&self) -> Result<T, ()> {
        let len = self.len.load(Acquire);
        if len == 0 {
            Err(())
        } else {
            let guard = self.guard.load(SeqCst);
            self.len.compare_and_swap(len, len - 1, Release);
            unsafe { Ok(ptr::read(guard.add(len))) }
        }
    }
}
pub fn pg<T>() -> PtrGuard<T> {
    PtrGuard::new()
}
pub fn pg_with<T>(cap: usize) -> PtrGuard<T> {
    PtrGuard::with_threads(cap)
}
