use std::alloc::{ Alloc, GlobalAlloc, Layout, Global, handle_alloc_error };
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::Condvar;
use std::sync::Once;

use super::num_threads;
pub use crate::{MapMootexGuard, Mootex, MootexGuard};
use super::own_ref::{Atomic, Shared, Owned, Pointer};

pub struct PtrGuard<T> {
    guard: Atomic<T>,
    cap: usize,
    len: AtomicUsize,
    lock: Mootex<()>,
}

impl<T: fmt::Debug> fmt::Debug for PtrGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut v = Vec::default();
        unsafe {
            let ptr = self.guard.load(SeqCst).as_raw();
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

unsafe fn atomic_ptr<T: fmt::Debug>(cap: usize) -> Atomic<T> {
    let align = mem::align_of::<T>();
        let item_size = mem::size_of::<T>();

        let ptr = Global.alloc(Layout::array::<T>(cap).unwrap());
        // this should only happen if T is misaligned or OOM
        if ptr.is_err() {
            handle_alloc_error(Layout::from_size_align_unchecked(
                cap * item_size,
                align
            ))
        }

        let ptr = ptr.unwrap();
        Atomic::from(ptr.as_ptr() as *const T)
}

impl<T: fmt::Debug> PtrGuard<T> {
    pub fn new() -> PtrGuard<T> {
        let cap = num_threads();
        let guard = unsafe { atomic_ptr::<T>(cap) };
        Self {
            guard,
            cap,
            len: AtomicUsize::new(0),
            lock: Mootex::default(),
        }
    }
    pub fn with_threads(threads: usize) -> PtrGuard<T> {
        let guard = unsafe { atomic_ptr::<T>(threads) };
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
    // pub fn push(&self, val: T) -> Result<(), T> {
    //     let len = self.len.load(Acquire);
    //     if len == self.cap {
    //         Err(val)
    //     } else {
    //         self.guard.push_back(val);
    //         Ok(())
    //     }
    // }
    // pub fn pop(&self) -> Result<Shared<T>, ()> {
    //     let len = self.len.load(Acquire);
    //     if len == 0 {
    //         Err(())
    //     } else {
    //         let res = self.guard.pop_front().unwrap();
    //         Ok(Shared::f(res))
    //     }
    // }
    pub fn push(&self, node: T) -> Result<(), T> {
        let len = self.len.load(Acquire);
        if len == self.cap {
            Err(node)
        } else {
            let guard = self.guard.load(SeqCst).as_raw_mut();
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
            let guard = self.guard.load(SeqCst).as_raw();
            self.len.compare_and_swap(len, len - 1, Release);
            unsafe { Ok(ptr::read(guard.add(len))) }
        }
    }
}
pub fn pg<T: fmt::Debug>() -> PtrGuard<T> {
    PtrGuard::new()
}
pub fn pg_with<T: fmt::Debug>(cap: usize) -> PtrGuard<T> {
    PtrGuard::with_threads(cap)
}
