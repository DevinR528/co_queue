use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Condvar;


mod deadlock;
mod mutex;
mod raw_mutex;

pub use mutex::{Mootex, MootexGuard, MapMootexGuard};


lazy_static::lazy_static! { static ref THREADS: usize = num_cpus::get(); }

pub(crate) struct Node<T> {
    /// raw pointer to data since every node is wrapped in an atomic.
    data: *const T,
    /// atomic pointers to next.
    next: AtomicPtr<Node<T>>,
    /// owns T.
    _mk: PhantomData<T>,
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Node {
            data: ptr::null_mut(),
            next: AtomicPtr::default(),
            _mk: PhantomData,
        }
    }
}

impl<T> Node<T> {
    fn new(val: T) -> Node<T> {
        let data = Box::into_raw(Box::new(val));
        Self {
            data,
            ..Default::default()
        }
    }
}

type LockNodePtr<T> = Mootex<AtomicPtr<Node<T>>>;
type NodePtr<T> = AtomicPtr<Node<T>>;

#[derive(Debug)]
pub(crate) struct PtrGuard<T>(LockNodePtr<T>);

impl<T> PtrGuard<T> {
    fn new() -> PtrGuard<T> {
        Self(Mootex::new(AtomicPtr::new(ptr::null_mut())))
    }
    fn lock(&self) -> MootexGuard<'_, NodePtr<T>> {
        self.0.lock()
    }
    fn unlock(&self) -> bool {
        true
    }
}
fn pg<T>() -> Vec<PtrGuard<T>> {
    let mut v = Vec::with_capacity(*THREADS);
    for _ in 0..v.capacity() {
        v.push(PtrGuard::new());
    }
    v
}

pub(crate) struct RawQueue<T> {
    /// Atomic pointer to a node that is the head.
    head: AtomicPtr<Node<T>>,
    /// Atomic pointer to a node that is the tail.
    tail: AtomicPtr<Node<T>>,
    /// TODO make len variable over threads
    /// The idea is to make a safe window around memory
    /// for mutation, insertion and/or removal.
    head_window: Vec<PtrGuard<T>>,
    /// Tails protection
    tail_window: Vec<PtrGuard<T>>,
}
impl<T> Default for RawQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> RawQueue<T> {
    fn new() -> RawQueue<T> {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            tail: AtomicPtr::new(ptr::null_mut()),
            head_window: pg::<T>(),
            tail_window: pg::<T>(),
        }
    }

    fn push(&self, val: T) -> bool {
        if self.head.load(Ordering::Acquire).is_null() {
            
        }
        true
    }
}

pub struct ShiftQueue<T> {
    raw: RawQueue<T>,
}

impl<T> Default for ShiftQueue<T> {
    fn default() -> Self {
        Self::with_threads(*THREADS)
    }
}

impl<T> ShiftQueue<T> {
    /// creates `ShiftQueue` that can be acted upon by `threads` number
    /// of threads. This is an optimization so getting the number
    /// wrong is not catastrophic.
    pub fn with_threads(theads: usize) -> ShiftQueue<T> {
        Self {
            raw: RawQueue::new(),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_ms_queue() {
        let que = ShiftQueue::<u8>::with_threads(2);
    }
}
