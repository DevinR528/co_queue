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
use own_ref::{Owned, Shared};
use shift_guard::{pg, pg_with, PtrGuard};
// lazy_static::lazy_static! { static ref THREADS: usize = num_cpus::get(); }

pub(super) static mut THREADS: usize = 0;
pub(super) static THREAD_COUNT: Once = Once::new();

pub(super) fn num_threads() -> usize {
    // this is safe as it will only ever be called once
    unsafe {
        THREAD_COUNT.call_once(|| {
            THREADS = num_cpus::get();
        });
        THREADS
    }
}

pub(crate) struct Node<T> {
    /// raw pointer to data since every node is wrapped in an atomic.
    data: ManuallyDrop<UnsafeCell<T>>,
    /// atomic pointers to next.
    next: AtomicPtr<Node<T>>,
}

impl<T: fmt::Debug> fmt::Debug for Node<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let next = if self.next().is_null() {
            &"null" as &dyn fmt::Debug
        } else {
            unsafe { &*self.next() as &dyn fmt::Debug }
        };
        let data = if self.data.get().is_null() {
            &"null" as &dyn fmt::Debug
        } else {
            unsafe { &*self.data.get() as &dyn fmt::Debug }
        };
        f.debug_struct("Node")
            .field("data", data)
            .field("next", next)
            .finish()
    }
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        let inner = unsafe { UnsafeCell::new(mem::uninitialized()) };
        let data = ManuallyDrop::new(inner);
        Node {
            data,
            next: AtomicPtr::default(),
        }
    }
}

impl<T: fmt::Debug> Node<T> {
    fn new(val: T) -> Node<T> {
        let inner = UnsafeCell::new(val);
        let data = ManuallyDrop::new(inner);
        Self {
            data,
            next: AtomicPtr::default(),
        }
    }
    fn as_mut_ptr(&mut self) -> *mut Node<T> {
        self as *mut _
    }

    fn next(&self) -> *mut Node<T> {
        self.next.load(Acquire)
    }
}

pub(crate) struct RawQueue<T> {
    /// Atomic pointer to a node that is the head.
    head: AtomicPtr<Node<T>>,
    /// Atomic pointer to a node that is the tail.
    tail: AtomicPtr<Node<T>>,
    /// TODO safe/better abstraction around PtrGuard<*const ...> yuck
    ///
    /// The idea is to make a safe window around memory
    /// for mutation, insertion and/or removal.
    push_guard: PtrGuard<*const Node<T>>,
    /// Tails protection
    pop_guard: PtrGuard<*const Node<T>>,
}

impl<T: fmt::Debug> Default for RawQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: fmt::Debug> fmt::Debug for RawQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut v = Vec::default();
        unsafe {
            let mut head = self.head.load(SeqCst);
            loop {
                if (&*head).next().is_null() {
                    v.push(&"null" as &dyn fmt::Debug);
                    break;
                };
                head = (*head).next.load(SeqCst);
                v.push(&*head as &dyn fmt::Debug);
            }
        }

        f.debug_struct("RawQueue")
            .field("push_guard", &self.push_guard)
            .field("pop_guard", &self.pop_guard)
            .field("data", &v)
            .finish()
    }
}
impl<T: fmt::Debug> RawQueue<T> {
    fn new() -> RawQueue<T> {
        // this can never be accessed as an actual Node<T>
        // it is UB but we need an empty node
        // TODO would Option work
        let mut empty = Node {
            data: unsafe { mem::uninitialized() },
            next: AtomicPtr::new(ptr::null_mut()),

        };

        let raw_q = Self {
            head: AtomicPtr::new(ptr::null_mut()),
            tail: AtomicPtr::new(ptr::null_mut()),
            push_guard: pg::<*const Node<T>>(),
            pop_guard: pg::<*const Node<T>>(),
        };

        let e_ptr = empty.as_mut_ptr();
        raw_q.head.store(e_ptr, Relaxed);
        raw_q.tail.store(e_ptr, Relaxed);
        raw_q
    }

    fn with_threads(threads: usize) -> RawQueue<T> {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            tail: AtomicPtr::new(ptr::null_mut()),
            push_guard: pg_with::<*const Node<T>>(threads),
            pop_guard: pg_with::<*const Node<T>>(threads),
        }
    }

    fn is_empty(&self) -> bool {
        let head = self.head.load(Acquire);
        let h = unsafe { &*head };
        h.next.load(Acquire).is_null()
    }

    unsafe fn _push(&self, mut new_node: Shared<'_, Node<T>>) -> bool {
        let mut tail = self.tail.load(Acquire);
        let mut head = self.head.load(Acquire);
        let next = (*tail).next();

        // if not null and the tail is the tail we have entered half way through an insert
        // we simply finish the process by making tail point correctly to new_node.
        if !next.is_null() {
            println!("not null");
            match self
                .tail
                .compare_exchange(tail, new_node.as_mut_ptr(), Release, Relaxed)
            {
                Ok(_null) => {
                    unsafe { println!("TAIL {:#?}", self.tail.load(SeqCst)) };
                    // swap head as well
                    match self
                        .head
                        .compare_exchange(head, new_node.as_mut_ptr(), Release, Relaxed)
                    {
                        Ok(_null) => {
                            unsafe { println!("HEAD {:#?}", self.head.load(SeqCst)) };
                            true
                        },
                        Err(_n) => {
                            println!("next null error");
                            if self.push_guard.push(new_node.as_raw()).is_err() {
                                panic!("push_guard failed to push")
                            };
                            false
                        }
                    }
                },
                Err(_n) => {
                    println!("next null error");
                    if self.push_guard.push(new_node.as_raw()).is_err() {
                        panic!("push_guard failed to push")
                    };
                    false
                }
            }
        } else {
            println!("in _push common path {:?}", new_node);
            // this is common path next is null so we swap new_node for it and shift tail to new_node
            if self.push_guard.is_empty() && (*tail).next().is_null() {
                match (*tail).next.compare_exchange(
                    ptr::null_mut(),
                    new_node.as_mut_ptr(),
                    Release,
                    Relaxed,
                ) {
                    Ok(_null) => {
                        println!("swap tail");
                        let res = self.tail.compare_exchange(
                            tail,
                            new_node.as_mut_ptr(),
                            Release,
                            Relaxed,
                        );
                        assert!(res.is_ok(), "swap to new tail failed");
                        unsafe { println!("HEAD {:#?}", self.head.load(SeqCst)) };
                        unsafe { println!("TAIL {:#?}", self.tail.load(SeqCst)) };
                        true
                    }
                    Err(_n) => {
                        println!("tail swap failed {:?}", _n);
                        false
                    }
                }
            } else {
                // because of failed bad concurrent pushes we have stolen nodes to push and must do so
                let _lock = self.push_guard.lock();
                println!("needed to use PtrGuard {:#?}", self);
                // these are failed pushes that have to be dealt with first
                while let Ok(gn) = self.push_guard.pop() {
                    // TODO safe?? yuck??
                    let guard_node: &mut Node<T> = &mut *(gn as *mut _);
                    if (*tail).next().is_null() {
                        match (*tail).next.compare_exchange(
                            (*tail).next(),
                            guard_node.as_mut_ptr(),
                            Release,
                            Relaxed,
                        ) {
                            Ok(_null) => {
                                let res = self.tail.compare_exchange(
                                    tail,
                                    guard_node.as_mut_ptr(),
                                    Release,
                                    Relaxed,
                                );
                                assert!(res.is_ok(), "swap to new tail failed");
                                tail = self.tail.load(Relaxed);
                                continue;
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
        println!("push {:?}", node);
        let new = node.into_shared();
        println!("push {:?}", new);
        unsafe { while !self._push(new) {} }
    }

    unsafe fn _pop(&self) -> Result<Option<T>, ()> {
        let head = self.head.load(Acquire);
        let next = (*head).next();

        if !next.is_null() {
            self.head
                .compare_exchange(head, next, Release, Relaxed)
                .map(|_| Some(ptr::read((*next).data.get())))
                .map_err(|_| {
                    println!("cmp_exc head with next _pop {:#?}", self);
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

///
#[derive(Debug)]
pub struct ShiftQueue<T> {
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
    fn test_ptr_guard() {
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
    fn push_try_pop_many_seq() {
        let q: ShiftQueue<u8> = ShiftQueue::new();
        assert!(q.is_empty());
        for i in 0..2 {
            q.push(i);
        }
        // println!("{:#?}", q);
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
}
