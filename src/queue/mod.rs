use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering::*};
use std::sync::Condvar;
use std::sync::Once;

mod own_ref;
mod shift_guard;

pub use crate::{MapMootexGuard, Mootex, MootexGuard};
use own_ref::{Atomic, Owned, Pointer, Shared};
use shift_guard::{pg, pg_with, PtrGuard};
// lazy_static::lazy_static! { static ref THREADS: usize = num_cpus::get(); }

pub(super) static mut THREADS: usize = 0;
pub(super) static THREAD_COUNT: Once = Once::new();

static mut PTR: usize = 0;

pub(super) fn num_threads() -> usize {
    // this is safe as it will only ever be called once
    unsafe {
        THREAD_COUNT.call_once(|| {
            THREADS = num_cpus::get();
        });
        THREADS
    }
}

pub(crate) struct Node<T: fmt::Debug> {
    /// raw pointer to data since every node is wrapped in an atomic.
    data: ManuallyDrop<T>,
    /// atomic pointers to next.
    next: Atomic<Node<T>>,
}

impl<T: fmt::Debug> fmt::Debug for Node<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let next = if self.next.load(SeqCst).is_null() {
            Box::new("null") as Box<dyn fmt::Debug>
        } else {
            let n = self.next.load(SeqCst);
            Box::new(n) as Box<dyn fmt::Debug>
        };
        let data = self.data.deref();
        f.debug_struct("Node")
            .field("data", data)
            .field("next", &next)
            .finish()
    }
}

impl<T: fmt::Debug> Drop for Node<T> {
    fn drop(&mut self) {
        println!("DROP NODE");
    }
}

impl<T: fmt::Debug> Default for Node<T> {
    #[allow(clippy::uninit_assumed_init)]
    fn default() -> Self {
        let data = unsafe { ManuallyDrop::new(MaybeUninit::uninit().assume_init()) };
        Node {
            data,
            next: Atomic::null(),
        }
    }
}

impl<T: fmt::Debug> Node<T> {
    fn new(val: T) -> Node<T> {
        let data = ManuallyDrop::new(val);
        Self {
            data,
            next: Atomic::null(),
        }
    }
    fn as_mut_ptr(&mut self) -> *mut Node<T> {
        self as *mut _
    }

    fn next(&self) -> Shared<'_, Node<T>> {
        self.next.load(Acquire)
    }
}

pub(crate) struct RawQueue<T: fmt::Debug> {
    /// Atomic pointer to a node that is the head.
    head: Atomic<Node<T>>,
    /// Atomic pointer to a node that is the tail.
    tail: Atomic<Node<T>>,
    /// The idea is to make a safe window around memory
    /// for mutation, insertion and/or removal.
    push_guard: PtrGuard<Owned<Node<T>>>,
    /// Tails protection
    pop_guard: PtrGuard<Owned<Node<T>>>,
}

impl<T: fmt::Debug> Default for RawQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: fmt::Debug> fmt::Debug for RawQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawQueue")
            .field("push_guard", &self.push_guard)
            .field("pop_guard", &self.pop_guard)
            .field("head", self.head.load(Relaxed).deref())
            .field("tail", self.tail.load(Relaxed).deref())
            .finish()
    }
}
impl<T: fmt::Debug> RawQueue<T> {
    fn new() -> RawQueue<T> {
        // this can never be accessed as an actual Node<T>
        // it is UB but we need an empty node
        let mut empty = Owned::new(Node::default());

        let raw_q = Self {
            head: Atomic::null(),
            tail: Atomic::null(),
            push_guard: pg::<Owned<Node<T>>>(),
            pop_guard: pg::<Owned<Node<T>>>(),
        };

        let e_ptr = empty.into_shared();
        raw_q.head.store(e_ptr, Relaxed);
        raw_q.tail.store(e_ptr, Relaxed);
        raw_q
    }

    fn with_threads(threads: usize) -> RawQueue<T> {
        // this can never be accessed as an actual Node<T>
        // it is UB but we need an empty node
        let mut empty = Owned::new(Node::default());

        let raw_q = Self {
            head: Atomic::null(),
            tail: Atomic::null(),
            push_guard: pg_with::<Owned<Node<T>>>(threads),
            pop_guard: pg_with::<Owned<Node<T>>>(threads),
        };

        let e_ptr = empty.into_shared();
        raw_q.head.store(e_ptr, Relaxed);
        raw_q.tail.store(e_ptr, Relaxed);
        raw_q
    }

    fn is_empty(&self) -> bool {
        let head = self.head.load(Acquire);
        let h = head.deref();
        h.next.load(Acquire).is_null()
    }

    unsafe fn _push(&self, mut new_node: Shared<'_, Node<T>>) -> bool {
        let mut tail = self.tail.load(Acquire);
        let next = tail.next();

        // if not null and the tail is the tail we have entered half way through an insert
        // we simply finish the process by making tail point correctly to new_node.
        if !next.is_null() {
            match self.tail.compare_set(tail, new_node, Release) {
                Ok(_null) => true,
                Err(_n) => {
                    if self.push_guard.push(new_node.into_owned()).is_err() {
                        panic!("push_guard failed to push {:#?}", self)
                    };
                    false
                }
            }
        } else {
            // this is common path next is null so we swap new_node for it and shift tail to new_node
            if self.push_guard.is_empty() && tail.next().is_null() {
                match tail.next.compare_set(Shared::null(), new_node, Release) {
                    Ok(_null) => {
                        let res = self.tail.compare_set(tail, new_node, Release);
                        assert!(res.is_ok(), "swap to new tail failed");
                        true
                    }
                    Err(_n) => {
                        if self.push_guard.push(new_node.into_owned()).is_err() {
                            panic!("push_guard failed to push {:#?}", self)
                        };
                        false
                    }
                }
            } else {
                // because of failed bad concurrent pushes we have stolen nodes to push and must do so
                let _lock = self.push_guard.lock();
                println!("needed to use PtrGuard {:#?}", self);
                // these are failed pushes that have to be dealt with first
                if let Ok(shared) = self.push_guard.pop().map(|n| n.into_shared()) {
                    // TODO safe?? yuck??
                    if tail.next().is_null() {
                        match tail.next.compare_set(tail.next(), shared, Release) {
                            Ok(_null) => {
                                let res = self.tail.compare_set(tail, shared, Release);
                                assert!(res.is_ok(), "swap to new tail failed");
                                return false;
                            }
                            Err(_n) => {
                                println!("cmp_exc tail and push_guard {:#?}", self);
                                return false;
                            }
                        }
                    }
                }
                // loop again to take common path
                false
            }
        }
    }

    fn push(&self, val: T) {
        let node = Owned::new(Node::new(val));
        let new = Owned::into_shared(node);

        unsafe { while !self._push(new) {} }
    }

    unsafe fn _pop(&self) -> Result<Option<T>, ()> {
        let head = self.head.load(Acquire);
        let next = (*head).next();

        if !next.is_null() {
            self.head
                .compare_set(head, next, Release)
                .map(|old_head| {
                    // add to pop_guard which acts as garbage collector
                    if self.pop_guard.push(old_head.into_owned()).is_err() {
                        // else lock and clear out garbage
                        let _lock = self.pop_guard.lock();
                        while let Ok(_popped) = self.pop_guard.pop() {
                            // TODO do i have to manually drop??
                        }
                        // push on to empty guard
                        assert!(self.pop_guard.push(old_head.into_owned()).is_ok());
                    };
                    Some(ptr::read((*next).data.deref()))
                })
                .map_err(|new_head| {
                    println!("cmp_set head with next _pop {:#?}", self);
                })
        } else {
            Ok(None)
        }
    }

    fn try_pop(&self) -> Option<T> {
        loop {
            if let Ok(t) = unsafe { self._pop() } {
                return t;
            }
        }
    }
}

impl<T: fmt::Debug> Drop for RawQueue<T> {
    fn drop(&mut self) {
        println!("DROP RAW QUEUE");
        while let Some(_) = self.try_pop() {}

        let head = self.head.load(Relaxed);
        drop(head.into_owned())
    }
}

///
#[derive(Debug)]
pub struct ShiftQueue<T: fmt::Debug> {
    raw: RawQueue<T>,
}

impl<T: fmt::Debug> Default for ShiftQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> ShiftQueue<T> {
    /// Creates a `ShiftQueue` with `PtrGuard` capacity of `num_cpus::get()`
    /// as the thread count.
    ///
    /// `ShiftQueue` can be acted upon by `threads` number
    /// of threads. This is an optimization so getting the number
    /// wrong is not catastrophic.
    pub fn new() -> ShiftQueue<T> {
        Self {
            raw: RawQueue::new(),
        }
    }
    /// Creates `ShiftQueue` that can be acted upon by `threads` number
    /// of threads. This is an optimization so getting the number
    /// wrong is not catastrophic.
    pub fn with_threads(threads: usize) -> ShiftQueue<T> {
        Self {
            raw: RawQueue::with_threads(threads),
        }
    }
    ///
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
    ///
    pub fn push(&self, val: T) {
        self.raw.push(val)
    }
    ///
    pub fn try_pop(&self) -> Option<T> {
        self.raw.try_pop()
    }
    ///
    #[must_use]
    pub fn pop(&self) -> T {
        loop {
            match self.raw.try_pop() {
                Some(t) => return t,
                None => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crossbeam_utils::thread;

    #[test]
    fn ptr_guard_queue() {
        let guard: PtrGuard<Node<u8>> = pg_with(4);
        for x in 0..4_u8 {
            assert!(guard.push(Node::new(x)).is_ok());
        }
        assert!(guard.push(Node::new(5)).is_err());
        assert!(guard.len() == 4);

        for _x in 0..4_u8 {
            assert!(guard.pop().is_ok());
        }

        assert!(guard.pop().is_err());
        assert!(guard.is_empty());
    }

    #[test]
    fn test_drop() {
        let ten = Box::new(10);
        {
            let q: ShiftQueue<Box<u8>> = ShiftQueue::new();
            q.push(ten);
        }
    }

    #[test]
    fn push_try_pop_many_seq() {
        let q: ShiftQueue<u8> = ShiftQueue::new();
        assert!(q.is_empty());
        for i in 0..2 {
            q.push(i);
        }
        println!("{:#?}", q);
        assert!(!q.is_empty());
        for i in 0..2 {
            assert_eq!(q.try_pop(), Some(i));
        }
        assert!(q.is_empty());
    }

    const CONC_COUNT: i64 = 1000000;

    #[test]
    fn push_pop_many_spsc() {
        let q: ShiftQueue<i64> = ShiftQueue::new();

        thread::scope(|scope| {
            scope.spawn(|_| {
                let mut next = 0;
                while next < CONC_COUNT {
                    assert_eq!(q.pop(), next);
                    next += 1;
                }
            });

            for i in 0..CONC_COUNT {
                q.push(i)
            }
        })
        .unwrap();
        assert!(q.is_empty());
    }

    #[test]
    fn exercise_push_pop_guards() {
        const COUNT: u32 = 100_u32;
        let q: ShiftQueue<u32> = ShiftQueue::with_threads(4);

        thread::scope(|scope| {
            // PUSH THREADS
            scope.spawn(|_| {
                for i in 0..COUNT {
                    q.push(i)
                }
            });
            scope.spawn(|_| {
                for i in 0..COUNT {
                    q.push(i)
                }
            });

            // POP THREADS
            scope.spawn(|_| {
                let mut next = 0;
                while next < COUNT {
                    assert_eq!(q.pop(), next);
                    next += 1;
                }
            });
            scope.spawn(|_| {
                let mut next = 0;
                while next < COUNT {
                    assert_eq!(q.pop(), next);
                    next += 1;
                }
            });
        })
        .unwrap();
        assert!(q.is_empty());
    }
}
