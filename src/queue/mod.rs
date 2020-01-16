use crossbeam::epoch::{self, Atomic, Guard, Owned, Shared, Pointer};
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
use shift_guard::{pg, pg_with, PtrGuard};
// use own_ref::{Owned, Shared};

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
    data: *const T,
    /// atomic pointers to next.
    next: Atomic<Node<T>>,
    /// owns T.
    _mk: PhantomData<T>,
}

impl<T: fmt::Debug> fmt::Debug for Node<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let next = if self.next.load(Relaxed, unsafe { &epoch::unprotected() }).is_null() {
            &"null" as &dyn fmt::Debug
        } else {
            unsafe { self.next.load(Relaxed, &epoch::unprotected()).deref() as &dyn fmt::Debug }
        };
        let data = if self.data.is_null() {
            &"null" as &dyn fmt::Debug
        } else {
            unsafe { &*self.data as &dyn fmt::Debug }
        };
        f.debug_struct("Node")
            .field("data", data)
            .field("next", next)
            .finish()
    }
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Node {
            data: ptr::null(),
            next: Atomic::null(),
            _mk: PhantomData,
        }
    }
}

impl<T: fmt::Debug> Node<T> {
    fn new(val: T) -> Node<T> {
        println!("being boxed {:?}", val);
        let data = Box::into_raw(Box::new(val));
        Self {
            data,
            next: Atomic::null(),
            _mk: PhantomData,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut Node<T> {
        self as *mut _
    }
}

pub(crate) struct RawQueue<T> {
    /// Atomic pointer to a node that is the head.
    head: Atomic<Node<T>>,
    /// Atomic pointer to a node that is the tail.
    tail: Atomic<Node<T>>,
    /// TODO safe/better abstraction around PtrGuard<*const ...> yuck
    ///
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
        //let mut v: Vec<&Node<T>> = Vec::default();
        let mut v = Vec::default();
        let guard = epoch::pin();
        unsafe {
            let mut head = self.head.load(SeqCst, &guard);
            loop {
                if head.deref().next.load(Relaxed, &guard).is_null() {
                    v.push(&"null" as &dyn fmt::Debug);
                    break;
                };
                head = head.deref().next.load(SeqCst, &guard);
                v.push(head.deref() as &dyn fmt::Debug);
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
        let mut empty = Owned::new(Node::default());

        let raw_q = Self {
            head: Atomic::null(),
            tail: Atomic::null(),
            push_guard: pg::<Owned<Node<T>>>(),
            pop_guard: pg::<Owned<Node<T>>>(),
        };
        let guard = unsafe { epoch::unprotected() };
        let e_shared = empty.into_shared(guard);

        raw_q.head.store(e_shared, Relaxed);
        raw_q.tail.store(e_shared, Relaxed);
        raw_q
    }

    fn with_threads(threads: usize) -> RawQueue<T> {
        let mut empty = Owned::new(Node::default());
        let raw_q = Self {
            head: Atomic::null(),
            tail: Atomic::null(),
            push_guard: pg_with::<Owned<Node<T>>>(threads),
            pop_guard: pg_with::<Owned<Node<T>>>(threads),
        };
        let guard = unsafe { epoch::unprotected() };
        let e_shared = empty.into_shared(guard);

        raw_q.head.store(e_shared, Relaxed);
        raw_q.tail.store(e_shared, Relaxed);
        raw_q
    }

    fn is_empty(&self) -> bool {
        let g = epoch::pin();
        let head = self.head.load(Acquire, &g);
        unsafe { head.deref().next.load(Acquire, &g).is_null() }
    }

    unsafe fn _push(&self, mut new_node: Shared<'_, Node<T>>, g: &Guard) -> bool {
        let mut tail = self.tail.load(Acquire, &g);
        let next = tail.deref().next.load(Acquire, &g);

        // if not null and the tail is the tail we have entered half way through an insert
        // we simply finish the process by making tail point correctly to new_node.
        if !next.is_null() {
            println!("next not null");
            match self.tail.compare_and_set(tail, new_node, Release, &g) {
                Ok(_null) => true,
                Err(_n) => {
                    println!("next null error");
                    if self.push_guard.push(new_node.into_owned(), g).is_err() {
                        panic!("push_guard failed to push")
                    };
                    false
                }
            }
        } else {
            // this is common path next is null so we swap new_node for it and shift tail to new_node
            if self.push_guard.is_empty() && tail.deref().next.load(Acquire, &g).is_null() {
                match tail.deref().next.compare_and_set(
                    Shared::null(),
                    new_node,
                    Release,
                    &g,
                ) {
                    Ok(_null) => {
                        let res = self.tail.compare_and_set(
                            tail,
                            new_node,
                            Release,
                            &g,
                        );
                        assert!(res.is_ok(), "swap to new tail failed");
                        true
                    }
                    Err(_n) => false,
                }
            } else {
                // because of failed bad concurrent pushes we have stolen nodes to push and must do so
                let _lock = self.push_guard.lock();
                println!("needed to use PtrGuard {:#?}", self);
                // these are failed pushes that have to be dealt with first
                while let Ok(gn) = self.push_guard.pop(g) {
                    let guard_node = gn.into_shared(g);
                    // TODO safe?? yuck??
                    if tail.deref().next.load(Acquire, g).is_null() {
                        match tail.deref().next.compare_and_set(
                            Shared::null(),
                            guard_node,
                            Release,
                            &g,
                        ) {
                            Ok(_null) => {
                                let res = self.tail.compare_and_set(
                                    tail,
                                    guard_node,
                                    Release,
                                    &g,
                                );
                                assert!(res.is_ok(), "swap to new tail failed");
                                tail = self.tail.load(Relaxed, &g);
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
        let g = epoch::pin();
        let node = Owned::new(Node::new(val));
        let new = Owned::into_shared(node, &g);

        unsafe { while !self._push(new, &g) {} }
    }

    fn _pop(&self, guard: &Guard) -> Result<Option<T>, ()> {
        let head = self.head.load(Acquire, guard);
        let h = unsafe { head.deref() };
        let next = h.next.load(Acquire, guard);
        match unsafe { next.as_ref() } {
            Some(n) => unsafe {
                self.head
                    .compare_and_set(head, next, Release, guard)
                    .map(|_| {
                        guard.defer_destroy(head);
                        Some(ptr::read(n.data))
                    })
                    .map_err(|_| ())
            },
            None => Ok(None),
        }
    }

    fn try_pop(&self) -> Option<T> {
        let g = epoch::pin();
        loop {
            if let Ok(t) = unsafe { self._pop(&g) } {
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

unsafe impl<T: Send> Send for ShiftQueue<T> {}
unsafe impl<T: Send> Sync for ShiftQueue<T> {}

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
        let g = epoch::pin();
        let guard: PtrGuard<Node<u8>> = pg_with(4);
        for x in 0..4_u8 {
            assert!(guard.push(Node::new(x), &g).is_ok());
        }
        assert!(guard.push(Node::new(5), &g).is_err());
        assert!(guard.len() == 4);

        for _x in 0..4_u8 {
            assert!(guard.pop(&g).is_ok());
        }

        assert!(guard.pop(&g).is_err());
        assert!(guard.is_empty());
    }

    #[test]
    fn push_try_pop_many_seq() {
        let q: ShiftQueue<u8> = ShiftQueue::new();
        assert!(q.is_empty());
        for i in 0..5 {
            q.push(i);
        }
        println!("{:#?}", q);
        assert!(!q.is_empty());
        for i in 0..5 {
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
