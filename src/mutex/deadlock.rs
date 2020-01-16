// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

// coppied to allow investigation of mutex, queue, and any other multi-threaded
// data structures

use std::{
    cell::{Cell, UnsafeCell},
    mem, ptr,
    sync::atomic::{fence, AtomicI32, AtomicPtr, AtomicUsize, Ordering},
};
use std::{
    thread,
    time::{Duration, Instant},
};

use libc;
pub(crate) use word_lock::SpinWait;

/// Acquire a resource identified by key in the deadlock detector
/// Noop if deadlock_detection feature isn't enabled.
///
/// # Safety
///
/// Call after the resource is acquired
#[inline]
pub unsafe fn acquire_resource(_key: usize) {
    deadlock_impl::acquire_resource(_key);
}

/// Release a resource identified by key in the deadlock detector.
/// Noop if deadlock_detection feature isn't enabled.
///
/// # Panics
///
/// Panics if the resource was already released or wasn't acquired in this thread.
///
/// # Safety
///
/// Call before the resource is released
#[inline]
pub unsafe fn release_resource(_key: usize) {
    deadlock_impl::release_resource(_key);
}

/// start of thread parker
fn TheadParker() {}

// x32 Linux uses a non-standard type for tv_nsec in timespec.
// See https://sourceware.org/bugzilla/show_bug.cgi?id=16437
#[cfg(all(target_arch = "x86_64", target_pointer_width = "32"))]
#[allow(non_camel_case_types)]
type tv_nsec_t = i64;
#[cfg(not(all(target_arch = "x86_64", target_pointer_width = "32")))]
#[allow(non_camel_case_types)]
type tv_nsec_t = libc::c_long;

fn errno() -> libc::c_int {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(target_os = "android")]
    unsafe {
        *libc::__errno()
    }
}

/// Trait for the platform thread parker implementation.
///
/// All unsafe methods are unsafe because the Unix thread parker is based on
/// pthread mutexes and condvars. Those primitives must not be moved and used
/// from any other memory address than the one they were located at when they
/// were initialized. As such, it's UB to call any unsafe method on
/// `ThreadParkerT` if the implementing instance has moved since the last
/// call to any of the unsafe methods.
pub trait ThreadParkerT {
    type UnparkHandle: UnparkHandleT;

    const IS_CHEAP_TO_CONSTRUCT: bool;

    fn new() -> Self;

    /// Prepares the parker. This should be called before adding it to the queue.
    unsafe fn prepare_park(&self);

    /// Checks if the park timed out. This should be called while holding the
    /// queue lock after park_until has returned false.
    unsafe fn timed_out(&self) -> bool;

    /// Parks the thread until it is unparked. This should be called after it has
    /// been added to the queue, after unlocking the queue.
    unsafe fn park(&self);

    /// Parks the thread until it is unparked or the timeout is reached. This
    /// should be called after it has been added to the queue, after unlocking
    /// the queue. Returns true if we were unparked and false if we timed out.
    unsafe fn park_until(&self, timeout: Instant) -> bool;

    /// Locks the parker to prevent the target thread from exiting. This is
    /// necessary to ensure that thread-local ThreadData objects remain valid.
    /// This should be called while holding the queue lock.
    unsafe fn unpark_lock(&self) -> Self::UnparkHandle;
}

/// Handle for a thread that is about to be unparked. We need to mark the thread
/// as unparked while holding the queue lock, but we delay the actual unparking
/// until after the queue lock is released.
pub trait UnparkHandleT {
    /// Wakes up the parked thread. This should be called after the queue lock is
    /// released to avoid blocking the queue for too long.
    ///
    /// This method is unsafe for the same reason as the unsafe methods in
    /// `ThreadParkerT`.
    unsafe fn unpark(self);
}

// Helper type for putting a thread to sleep until some other thread wakes it up
pub struct ThreadParker {
    futex: AtomicI32,
}

impl ThreadParkerT for ThreadParker {
    type UnparkHandle = UnparkHandle;

    const IS_CHEAP_TO_CONSTRUCT: bool = true;

    #[inline]
    fn new() -> ThreadParker {
        ThreadParker {
            futex: AtomicI32::new(0),
        }
    }

    #[inline]
    unsafe fn prepare_park(&self) {
        self.futex.store(1, Ordering::Relaxed);
    }

    #[inline]
    unsafe fn timed_out(&self) -> bool {
        self.futex.load(Ordering::Relaxed) != 0
    }

    #[inline]
    unsafe fn park(&self) {
        while self.futex.load(Ordering::Acquire) != 0 {
            self.futex_wait(None);
        }
    }

    #[inline]
    unsafe fn park_until(&self, timeout: Instant) -> bool {
        while self.futex.load(Ordering::Acquire) != 0 {
            let now = Instant::now();
            if timeout <= now {
                return false;
            }
            let diff = timeout - now;
            if diff.as_secs() as libc::time_t as u64 != diff.as_secs() {
                // Timeout overflowed, just sleep indefinitely
                self.park();
                return true;
            }
            let ts = libc::timespec {
                tv_sec: diff.as_secs() as libc::time_t,
                tv_nsec: diff.subsec_nanos() as tv_nsec_t,
            };
            self.futex_wait(Some(ts));
        }
        true
    }

    // Locks the parker to prevent the target thread from exiting. This is
    // necessary to ensure that thread-local ThreadData objects remain valid.
    // This should be called while holding the queue lock.
    #[inline]
    unsafe fn unpark_lock(&self) -> UnparkHandle {
        // We don't need to lock anything, just clear the state
        self.futex.store(0, Ordering::Release);

        UnparkHandle { futex: &self.futex }
    }
}

impl ThreadParker {
    #[inline]
    fn futex_wait(&self, ts: Option<libc::timespec>) {
        let ts_ptr = ts
            .as_ref()
            .map(|ts_ref| ts_ref as *const _)
            .unwrap_or(ptr::null());
        let r = unsafe {
            libc::syscall(
                libc::SYS_futex,
                &self.futex,
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                1,
                ts_ptr,
            )
        };
        debug_assert!(r == 0 || r == -1);
        if r == -1 {
            debug_assert!(
                errno() == libc::EINTR
                    || errno() == libc::EAGAIN
                    || (ts.is_some() && errno() == libc::ETIMEDOUT)
            );
        }
    }
}

pub struct UnparkHandle {
    futex: *const AtomicI32,
}

impl UnparkHandleT for UnparkHandle {
    #[inline]
    unsafe fn unpark(self) {
        // The thread data may have been freed at this point, but it doesn't
        // matter since the syscall will just return EFAULT in that case.
        let r = libc::syscall(
            libc::SYS_futex,
            self.futex,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            1,
        );
        debug_assert!(r == 0 || r == 1 || r == -1);
        if r == -1 {
            debug_assert_eq!(errno(), libc::EFAULT);
        }
    }
}

/// contains SpinWait and WordLock.
mod word_lock {

    use super::{
        fence, mem, ptr, thread, AtomicUsize, Cell, Ordering, ThreadParker, ThreadParkerT,
        UnparkHandleT,
    };
    use std::sync::atomic::spin_loop_hint;

    // Wastes some CPU time for the given number of iterations,
    // using a hint to indicate to the CPU that we are spinning.
    #[inline]
    fn cpu_relax(iterations: u32) {
        for _ in 0..iterations {
            spin_loop_hint()
        }
    }

    #[inline]
    pub fn thread_yield() {
        thread::yield_now();
    }

    /// A counter used to perform exponential backoff in spin loops.
    #[derive(Default)]
    pub struct SpinWait {
        counter: u32,
    }

    impl SpinWait {
        /// Creates a new `SpinWait`.
        #[inline]
        pub fn new() -> Self {
            Self::default()
        }

        /// Resets a `SpinWait` to its initial state.
        #[inline]
        pub fn reset(&mut self) {
            self.counter = 0;
        }

        /// Spins until the sleep threshold has been reached.
        ///
        /// This function returns whether the sleep threshold has been reached, at
        /// which point further spinning has diminishing returns and the thread
        /// should be parked instead.
        ///
        /// The spin strategy will initially use a CPU-bound loop but will fall back
        /// to yielding the CPU to the OS after a few iterations.
        #[inline]
        pub fn spin(&mut self) -> bool {
            if self.counter >= 10 {
                return false;
            }
            self.counter += 1;
            if self.counter <= 3 {
                cpu_relax(1 << self.counter);
            } else {
                thread_yield();
            }
            true
        }
    }

    struct ThreadData {
        parker: ThreadParker,

        // Linked list of threads in the queue. The queue is split into two parts:
        // the processed part and the unprocessed part. When new nodes are added to
        // the list, they only have the next pointer set, and queue_tail is null.
        //
        // Nodes are processed with the queue lock held, which consists of setting
        // the prev pointer for each node and setting the queue_tail pointer on the
        // first processed node of the list.
        //
        // This setup allows nodes to be added to the queue without a lock, while
        // still allowing O(1) removal of nodes from the processed part of the list.
        // The only cost is the O(n) processing, but this only needs to be done
        // once for each node, and therefore isn't too expensive.
        queue_tail: Cell<*const ThreadData>,
        prev: Cell<*const ThreadData>,
        next: Cell<*const ThreadData>,
    }

    impl ThreadData {
        #[inline]
        fn new() -> ThreadData {
            assert!(mem::align_of::<ThreadData>() > !QUEUE_MASK);
            ThreadData {
                parker: ThreadParker::new(),
                queue_tail: Cell::new(ptr::null()),
                prev: Cell::new(ptr::null()),
                next: Cell::new(ptr::null()),
            }
        }
    }

    // Invokes the given closure with a reference to the current thread `ThreadData`.
    #[inline]
    fn with_thread_data<T>(f: impl FnOnce(&ThreadData) -> T) -> T {
        let mut thread_data_ptr = ptr::null();
        // If ThreadData is expensive to construct, then we want to use a cached
        // version in thread-local storage if possible.
        if !ThreadParker::IS_CHEAP_TO_CONSTRUCT {
            thread_local!(static THREAD_DATA: ThreadData = ThreadData::new());
            if let Ok(tls_thread_data) = THREAD_DATA.try_with(|x| x as *const ThreadData) {
                thread_data_ptr = tls_thread_data;
            }
        }
        // Otherwise just create a ThreadData on the stack
        let mut thread_data_storage = None;
        if thread_data_ptr.is_null() {
            thread_data_ptr = thread_data_storage.get_or_insert_with(ThreadData::new);
        }

        f(unsafe { &*thread_data_ptr })
    }

    const LOCKED_BIT: usize = 1;
    const QUEUE_LOCKED_BIT: usize = 2;
    const QUEUE_MASK: usize = !3;

    // Word-sized lock that is used to implement the parking_lot API. Since this
    // can't use parking_lot, it instead manages its own queue of waiting threads.
    pub struct WordLock {
        state: AtomicUsize,
    }

    impl WordLock {
        /// Returns a new, unlocked, WordLock.
        pub const fn new() -> Self {
            WordLock {
                state: AtomicUsize::new(0),
            }
        }

        #[inline]
        pub fn lock(&self) {
            if self
                .state
                .compare_exchange_weak(0, LOCKED_BIT, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            self.lock_slow();
        }

        /// Must not be called on an already unlocked `WordLock`!
        #[inline]
        pub unsafe fn unlock(&self) {
            let state = self.state.fetch_sub(LOCKED_BIT, Ordering::Release);
            if state.is_queue_locked() || state.queue_head().is_null() {
                return;
            }
            self.unlock_slow();
        }

        #[cold]
        fn lock_slow(&self) {
            let mut spinwait = SpinWait::new();
            let mut state = self.state.load(Ordering::Relaxed);
            loop {
                // Grab the lock if it isn't locked, even if there is a queue on it
                if !state.is_locked() {
                    match self.state.compare_exchange_weak(
                        state,
                        state | LOCKED_BIT,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(x) => state = x,
                    }
                    continue;
                }

                // If there is no queue, try spinning a few times
                if state.queue_head().is_null() && spinwait.spin() {
                    state = self.state.load(Ordering::Relaxed);
                    continue;
                }

                // Get our thread data and prepare it for parking
                state = with_thread_data(|thread_data| {
                    // The pthread implementation is still unsafe, so we need to surround `prepare_park`
                    // with `unsafe {}`.
                    #[allow(unused_unsafe)]
                    unsafe {
                        thread_data.parker.prepare_park();
                    }

                    // Add our thread to the front of the queue
                    let queue_head = state.queue_head();
                    if queue_head.is_null() {
                        thread_data.queue_tail.set(thread_data);
                        thread_data.prev.set(ptr::null());
                    } else {
                        thread_data.queue_tail.set(ptr::null());
                        thread_data.prev.set(ptr::null());
                        thread_data.next.set(queue_head);
                    }
                    if let Err(x) = self.state.compare_exchange_weak(
                        state,
                        state.with_queue_head(thread_data),
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        return x;
                    }

                    // Sleep until we are woken up by an unlock
                    // Ignoring unused unsafe, since it's only a few platforms where this is unsafe.
                    #[allow(unused_unsafe)]
                    unsafe {
                        thread_data.parker.park();
                    }

                    // Loop back and try locking again
                    spinwait.reset();
                    self.state.load(Ordering::Relaxed)
                });
            }
        }

        #[cold]
        fn unlock_slow(&self) {
            let mut state = self.state.load(Ordering::Relaxed);
            loop {
                // We just unlocked the WordLock. Just check if there is a thread
                // to wake up. If the queue is locked then another thread is already
                // taking care of waking up a thread.
                if state.is_queue_locked() || state.queue_head().is_null() {
                    return;
                }

                // Try to grab the queue lock
                match self.state.compare_exchange_weak(
                    state,
                    state | QUEUE_LOCKED_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => state = x,
                }
            }

            // Now we have the queue lock and the queue is non-empty
            'outer: loop {
                // First, we need to fill in the prev pointers for any newly added
                // threads. We do this until we reach a node that we previously
                // processed, which has a non-null queue_tail pointer.
                let queue_head = state.queue_head();
                let mut queue_tail;
                let mut current = queue_head;
                loop {
                    queue_tail = unsafe { (*current).queue_tail.get() };
                    if !queue_tail.is_null() {
                        break;
                    }
                    unsafe {
                        let next = (*current).next.get();
                        (*next).prev.set(current);
                        current = next;
                    }
                }

                // Set queue_tail on the queue head to indicate that the whole list
                // has prev pointers set correctly.
                unsafe {
                    (*queue_head).queue_tail.set(queue_tail);
                }

                // If the WordLock is locked, then there is no point waking up a
                // thread now. Instead we let the next unlocker take care of waking
                // up a thread.
                if state.is_locked() {
                    match self.state.compare_exchange_weak(
                        state,
                        state & !QUEUE_LOCKED_BIT,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(x) => state = x,
                    }

                    // Need an acquire fence before reading the new queue
                    fence(Ordering::Acquire);
                    continue;
                }

                // Remove the last thread from the queue and unlock the queue
                let new_tail = unsafe { (*queue_tail).prev.get() };
                if new_tail.is_null() {
                    loop {
                        match self.state.compare_exchange_weak(
                            state,
                            state & LOCKED_BIT,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(x) => state = x,
                        }

                        // If the compare_exchange failed because a new thread was
                        // added to the queue then we need to re-scan the queue to
                        // find the previous element.
                        if state.queue_head().is_null() {
                            continue;
                        } else {
                            // Need an acquire fence before reading the new queue
                            fence(Ordering::Acquire);
                            continue 'outer;
                        }
                    }
                } else {
                    unsafe {
                        (*queue_head).queue_tail.set(new_tail);
                    }
                    self.state.fetch_and(!QUEUE_LOCKED_BIT, Ordering::Release);
                }

                // Finally, wake up the thread we removed from the queue. Note that
                // we don't need to worry about any races here since the thread is
                // guaranteed to be sleeping right now and we are the only one who
                // can wake it up.
                unsafe {
                    (*queue_tail).parker.unpark_lock().unpark();
                }
                break;
            }
        }
    }

    trait LockState {
        fn is_locked(self) -> bool;
        fn is_queue_locked(self) -> bool;
        fn queue_head(self) -> *const ThreadData;
        fn with_queue_head(self, thread_data: *const ThreadData) -> Self;
    }

    impl LockState for usize {
        #[inline]
        fn is_locked(self) -> bool {
            self & LOCKED_BIT != 0
        }

        #[inline]
        fn is_queue_locked(self) -> bool {
            self & QUEUE_LOCKED_BIT != 0
        }

        #[inline]
        fn queue_head(self) -> *const ThreadData {
            (self & QUEUE_MASK) as *const ThreadData
        }

        #[inline]
        fn with_queue_head(self, thread_data: *const ThreadData) -> Self {
            (self & !QUEUE_MASK) | thread_data as *const _ as usize
        }
    }
}

static NUM_THREADS: AtomicUsize = AtomicUsize::new(0);
/// Holds the pointer to the currently active `HashTable`.
///
/// # Safety
///
/// Except for the initial value of null, it must always point to a valid `HashTable` instance.
/// Any `HashTable` this global static has ever pointed to must never be freed.
static HASHTABLE: AtomicPtr<HashTable> = AtomicPtr::new(ptr::null_mut());
// Even with 3x more buckets than threads, the memory overhead per thread is
// still only a few hundred bytes per thread.
const LOAD_FACTOR: usize = 3;

struct HashTable {
    // Hash buckets for the table
    entries: Box<[Bucket]>,

    // Number of bits used for the hash function
    hash_bits: u32,

    // Previous table. This is only kept to keep leak detectors happy.
    _prev: *const HashTable,
}
impl HashTable {
    #[inline]
    fn new(num_threads: usize, prev: *const HashTable) -> Box<HashTable> {
        let new_size = (num_threads * LOAD_FACTOR).next_power_of_two();
        let hash_bits = 0usize.leading_zeros() - new_size.leading_zeros() - 1;

        let now = Instant::now();
        let mut entries = Vec::with_capacity(new_size);
        for i in 0..new_size {
            // We must ensure the seed is not zero
            entries.push(Bucket::new(now, i as u32 + 1));
        }

        Box::new(HashTable {
            entries: entries.into_boxed_slice(),
            hash_bits,
            _prev: prev,
        })
    }
}
#[repr(align(64))]
struct Bucket {
    // Lock protecting the queue
    mutex: word_lock::WordLock,

    // Linked list of threads waiting on this bucket
    queue_head: Cell<*const ThreadData>,
    queue_tail: Cell<*const ThreadData>,

    // Next time at which point be_fair should be set
    fair_timeout: UnsafeCell<FairTimeout>,
}

impl Bucket {
    #[inline]
    pub fn new(timeout: Instant, seed: u32) -> Self {
        Self {
            mutex: word_lock::WordLock::new(),
            queue_head: Cell::new(ptr::null()),
            queue_tail: Cell::new(ptr::null()),
            fair_timeout: UnsafeCell::new(FairTimeout::new(timeout, seed)),
        }
    }
}

struct FairTimeout {
    // Next time at which point be_fair should be set
    timeout: Instant,

    // the PRNG state for calculating the next timeout
    seed: u32,
}

impl FairTimeout {
    #[inline]
    fn new(timeout: Instant, seed: u32) -> FairTimeout {
        FairTimeout { timeout, seed }
    }

    // Determine whether we should force a fair unlock, and update the timeout
    #[inline]
    fn should_timeout(&mut self) -> bool {
        let now = Instant::now();
        if now > self.timeout {
            // Time between 0 and 1ms.
            let nanos = self.gen_u32() % 1_000_000;
            self.timeout = now + Duration::new(0, nanos);
            true
        } else {
            false
        }
    }

    // Pseudorandom number generator from the "Xorshift RNGs" paper by George Marsaglia.
    fn gen_u32(&mut self) -> u32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        self.seed
    }
}

struct ThreadData {
    parker: ThreadParker,

    // Key that this thread is sleeping on. This may change if the thread is
    // requeued to a different key.
    key: AtomicUsize,

    // Linked list of parked threads in a bucket
    next_in_queue: Cell<*const ThreadData>,

    // UnparkToken passed to this thread when it is unparked
    unpark_token: Cell<UnparkToken>,

    // ParkToken value set by the thread when it was parked
    park_token: Cell<ParkToken>,

    // Is the thread parked with a timeout?
    parked_with_timeout: Cell<bool>,

    // Extra data for deadlock detection
    deadlock_data: deadlock_impl::DeadlockData,
}

impl ThreadData {
    fn new() -> ThreadData {
        // Keep track of the total number of live ThreadData objects and resize
        // the hash table accordingly.
        let num_threads = NUM_THREADS.fetch_add(1, Ordering::Relaxed) + 1;
        grow_hashtable(num_threads);

        ThreadData {
            parker: ThreadParker::new(),
            key: AtomicUsize::new(0),
            next_in_queue: Cell::new(ptr::null()),
            unpark_token: Cell::new(DEFAULT_UNPARK_TOKEN),
            park_token: Cell::new(DEFAULT_PARK_TOKEN),
            parked_with_timeout: Cell::new(false),
            deadlock_data: deadlock_impl::DeadlockData::new(),
        }
    }
}

// Invokes the given closure with a reference to the current thread `ThreadData`.
#[inline(always)]
fn with_thread_data<T>(f: impl FnOnce(&ThreadData) -> T) -> T {
    // Unlike word_lock::ThreadData, parking_lot::ThreadData is always expensive
    // to construct. Try to use a thread-local version if possible. Otherwise just
    // create a ThreadData on the stack
    let mut thread_data_storage = None;
    thread_local!(static THREAD_DATA: ThreadData = ThreadData::new());
    let thread_data_ptr = THREAD_DATA
        .try_with(|x| x as *const ThreadData)
        .unwrap_or_else(|_| thread_data_storage.get_or_insert_with(ThreadData::new));

    f(unsafe { &*thread_data_ptr })
}

impl Drop for ThreadData {
    fn drop(&mut self) {
        NUM_THREADS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Returns a reference to the latest hash table, creating one if it doesn't exist yet.
/// The reference is valid forever. However, the `HashTable` it references might become stale
/// at any point. Meaning it still exists, but it is not the instance in active use.
#[inline]
fn get_hashtable() -> &'static HashTable {
    let table = HASHTABLE.load(Ordering::Acquire);

    // If there is no table, create one
    if table.is_null() {
        create_hashtable()
    } else {
        // SAFETY: when not null, `HASHTABLE` always points to a `HashTable` that is never freed.
        unsafe { &*table }
    }
}

/// Returns a reference to the latest hash table, creating one if it doesn't exist yet.
/// The reference is valid forever. However, the `HashTable` it references might become stale
/// at any point. Meaning it still exists, but it is not the instance in active use.
#[cold]
fn create_hashtable() -> &'static HashTable {
    let new_table = Box::into_raw(HashTable::new(LOAD_FACTOR, ptr::null()));

    // If this fails then it means some other thread created the hash table first.
    let table = match HASHTABLE.compare_exchange(
        ptr::null_mut(),
        new_table,
        Ordering::Release,
        Ordering::Relaxed,
    ) {
        Ok(_) => new_table,
        Err(old_table) => {
            // Free the table we created
            // SAFETY: `new_table` is created from `Box::into_raw` above and only freed here.
            unsafe {
                Box::from_raw(new_table);
            }
            old_table
        }
    };
    // SAFETY: The `HashTable` behind `table` is never freed. It is either the table pointer we
    // created here, or it is one loaded from `HASHTABLE`.
    unsafe { &*table }
}

// Grow the hash table so that it is big enough for the given number of threads.
// This isn't performance-critical since it is only done when a ThreadData is
// created, which only happens once per thread.
fn grow_hashtable(num_threads: usize) {
    // Lock all buckets in the existing table and get a reference to it
    let old_table = loop {
        let table = get_hashtable();

        // Check if we need to resize the existing table
        if table.entries.len() >= LOAD_FACTOR * num_threads {
            return;
        }

        // Lock all buckets in the old table
        for bucket in &table.entries[..] {
            bucket.mutex.lock();
        }

        // Now check if our table is still the latest one. Another thread could
        // have grown the hash table between us reading HASHTABLE and locking
        // the buckets.
        if HASHTABLE.load(Ordering::Relaxed) == table as *const _ as *mut _ {
            break table;
        }

        // Unlock buckets and try again
        for bucket in &table.entries[..] {
            // SAFETY: We hold the lock here, as required
            unsafe { bucket.mutex.unlock() };
        }
    };

    // Create the new table
    let mut new_table = HashTable::new(num_threads, old_table);

    // Move the entries from the old table to the new one
    for bucket in &old_table.entries[..] {
        // SAFETY: The park, unpark* and check_wait_graph_fast functions create only correct linked
        // lists. All `ThreadData` instances in these lists will remain valid as long as they are
        // present in the lists, meaning as long as their threads are parked.
        unsafe { rehash_bucket_into(bucket, &mut new_table) };
    }

    // Publish the new table. No races are possible at this point because
    // any other thread trying to grow the hash table is blocked on the bucket
    // locks in the old table.
    HASHTABLE.store(Box::into_raw(new_table), Ordering::Release);

    // Unlock all buckets in the old table
    for bucket in &old_table.entries[..] {
        // SAFETY: We hold the lock here, as required
        unsafe { bucket.mutex.unlock() };
    }
}

/// Iterate through all `ThreadData` objects in the bucket and insert them into the given table
/// in the bucket their key correspond to for this table.
///
/// # Safety
///
/// The given `bucket` must have a correctly constructed linked list under `queue_head`, containing
/// `ThreadData` instances that must stay valid at least as long as the given `table` is in use.
///
/// The given `table` must only contain buckets with correctly constructed linked lists.
unsafe fn rehash_bucket_into(bucket: &'static Bucket, table: &mut HashTable) {
    let mut current: *const ThreadData = bucket.queue_head.get();
    while !current.is_null() {
        let next = (*current).next_in_queue.get();
        let hash = hash((*current).key.load(Ordering::Relaxed), table.hash_bits);
        if table.entries[hash].queue_tail.get().is_null() {
            table.entries[hash].queue_head.set(current);
        } else {
            (*table.entries[hash].queue_tail.get())
                .next_in_queue
                .set(current);
        }
        table.entries[hash].queue_tail.set(current);
        (*current).next_in_queue.set(ptr::null());
        current = next;
    }
}

// Hash function for addresses
#[cfg(target_pointer_width = "32")]
#[inline]
fn hash(key: usize, bits: u32) -> usize {
    key.wrapping_mul(0x9E3779B9) >> (32 - bits)
}
#[cfg(target_pointer_width = "64")]
#[inline]
fn hash(key: usize, bits: u32) -> usize {
    key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)
}

/// Locks the bucket for the given key and returns a reference to it.
/// The returned bucket must be unlocked again in order to not cause deadlocks.
#[inline]
fn lock_bucket(key: usize) -> &'static Bucket {
    loop {
        let hashtable = get_hashtable();

        let hash = hash(key, hashtable.hash_bits);
        let bucket = &hashtable.entries[hash];

        // Lock the bucket
        bucket.mutex.lock();

        // If no other thread has rehashed the table before we grabbed the lock
        // then we are good to go! The lock we grabbed prevents any rehashes.
        if HASHTABLE.load(Ordering::Relaxed) == hashtable as *const _ as *mut _ {
            return bucket;
        }

        // Unlock the bucket and try again
        // SAFETY: We hold the lock here, as required
        unsafe { bucket.mutex.unlock() };
    }
}

/// Locks the bucket for the given key and returns a reference to it. But checks that the key
/// hasn't been changed in the meantime due to a requeue.
/// The returned bucket must be unlocked again in order to not cause deadlocks.
#[inline]
fn lock_bucket_checked(key: &AtomicUsize) -> (usize, &'static Bucket) {
    loop {
        let hashtable = get_hashtable();
        let current_key = key.load(Ordering::Relaxed);

        let hash = hash(current_key, hashtable.hash_bits);
        let bucket = &hashtable.entries[hash];

        // Lock the bucket
        bucket.mutex.lock();

        // Check that both the hash table and key are correct while the bucket
        // is locked. Note that the key can't change once we locked the proper
        // bucket for it, so we just keep trying until we have the correct key.
        if HASHTABLE.load(Ordering::Relaxed) == hashtable as *const _ as *mut _
            && key.load(Ordering::Relaxed) == current_key
        {
            return (current_key, bucket);
        }

        // Unlock the bucket and try again
        // SAFETY: We hold the lock here, as required
        unsafe { bucket.mutex.unlock() };
    }
}

/// Result of a park operation.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ParkResult {
    /// We were unparked by another thread with the given token.
    Unparked(UnparkToken),

    /// The validation callback returned false.
    Invalid,

    /// The timeout expired.
    TimedOut,
}

/// Result of an unpark operation.
#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub struct UnparkResult {
    /// The number of threads that were unparked.
    pub unparked_threads: usize,

    /// The number of threads that were requeued.
    pub requeued_threads: usize,

    /// Whether there are any threads remaining in the queue. This only returns
    /// true if a thread was unparked.
    pub have_more_threads: bool,

    /// This is set to true on average once every 0.5ms for any given key. It
    /// should be used to switch to a fair unlocking mechanism for a particular
    /// unlock.
    pub be_fair: bool,

    /// Private field so new fields can be added without breakage.
    _sealed: (),
}

/// A value which is passed from an unparker to a parked thread.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct UnparkToken(pub usize);

/// A value associated with a parked thread which can be used by `unpark_filter`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct ParkToken(pub usize);

/// A default unpark token to use.
pub const DEFAULT_UNPARK_TOKEN: UnparkToken = UnparkToken(0);

/// A default park token to use.
pub const DEFAULT_PARK_TOKEN: ParkToken = ParkToken(0);

/// Parks the current thread in the queue associated with the given key.
///
/// The `validate` function is called while the queue is locked and can abort
/// the operation by returning false. If `validate` returns true then the
/// current thread is appended to the queue and the queue is unlocked.
///
/// The `before_sleep` function is called after the queue is unlocked but before
/// the thread is put to sleep. The thread will then sleep until it is unparked
/// or the given timeout is reached.
///
/// The `timed_out` function is also called while the queue is locked, but only
/// if the timeout was reached. It is passed the key of the queue it was in when
/// it timed out, which may be different from the original key if
/// `unpark_requeue` was called. It is also passed a bool which indicates
/// whether it was the last thread in the queue.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `validate` and `timed_out` functions are called while the queue is
/// locked and must not panic or call into any function in `parking_lot`.
///
/// The `before_sleep` function is called outside the queue lock and is allowed
/// to call `unpark_one`, `unpark_all`, `unpark_requeue` or `unpark_filter`, but
/// it is not allowed to call `park` or panic.
#[inline]
pub unsafe fn park(
    key: usize,
    validate: impl FnOnce() -> bool,
    before_sleep: impl FnOnce(),
    timed_out: impl FnOnce(usize, bool),
    park_token: ParkToken,
    timeout: Option<Instant>,
) -> ParkResult {
    // Grab our thread data, this also ensures that the hash table exists
    with_thread_data(|thread_data| {
        // Lock the bucket for the given key
        let bucket = lock_bucket(key);

        // If the validation function fails, just return
        if !validate() {
            // SAFETY: We hold the lock here, as required
            bucket.mutex.unlock();
            return ParkResult::Invalid;
        }

        // Append our thread data to the queue and unlock the bucket
        thread_data.parked_with_timeout.set(timeout.is_some());
        thread_data.next_in_queue.set(ptr::null());
        thread_data.key.store(key, Ordering::Relaxed);
        thread_data.park_token.set(park_token);
        thread_data.parker.prepare_park();
        if !bucket.queue_head.get().is_null() {
            (*bucket.queue_tail.get()).next_in_queue.set(thread_data);
        } else {
            bucket.queue_head.set(thread_data);
        }
        bucket.queue_tail.set(thread_data);
        // SAFETY: We hold the lock here, as required
        bucket.mutex.unlock();

        // Invoke the pre-sleep callback
        before_sleep();

        // Park our thread and determine whether we were woken up by an unpark
        // or by our timeout. Note that this isn't precise: we can still be
        // unparked since we are still in the queue.
        let unparked = match timeout {
            Some(timeout) => thread_data.parker.park_until(timeout),
            None => {
                thread_data.parker.park();
                // call deadlock detection on_unpark hook
                deadlock_impl::on_unpark(thread_data);
                true
            }
        };

        // If we were unparked, return now
        if unparked {
            return ParkResult::Unparked(thread_data.unpark_token.get());
        }

        // Lock our bucket again. Note that the hashtable may have been rehashed in
        // the meantime. Our key may also have changed if we were requeued.
        let (key, bucket) = lock_bucket_checked(&thread_data.key);

        // Now we need to check again if we were unparked or timed out. Unlike the
        // last check this is precise because we hold the bucket lock.
        if !thread_data.parker.timed_out() {
            // SAFETY: We hold the lock here, as required
            bucket.mutex.unlock();
            return ParkResult::Unparked(thread_data.unpark_token.get());
        }

        // We timed out, so we now need to remove our thread from the queue
        let mut link = &bucket.queue_head;
        let mut current = bucket.queue_head.get();
        let mut previous = ptr::null();
        let mut was_last_thread = true;
        while !current.is_null() {
            if current == thread_data {
                let next = (*current).next_in_queue.get();
                link.set(next);
                if bucket.queue_tail.get() == current {
                    bucket.queue_tail.set(previous);
                } else {
                    // Scan the rest of the queue to see if there are any other
                    // entries with the given key.
                    let mut scan = next;
                    while !scan.is_null() {
                        if (*scan).key.load(Ordering::Relaxed) == key {
                            was_last_thread = false;
                            break;
                        }
                        scan = (*scan).next_in_queue.get();
                    }
                }

                // Callback to indicate that we timed out, and whether we were the
                // last thread on the queue.
                timed_out(key, was_last_thread);
                break;
            } else {
                if (*current).key.load(Ordering::Relaxed) == key {
                    was_last_thread = false;
                }
                link = &(*current).next_in_queue;
                previous = current;
                current = link.get();
            }
        }

        // There should be no way for our thread to have been removed from the queue
        // if we timed out.
        debug_assert!(!current.is_null());

        // Unlock the bucket, we are done
        // SAFETY: We hold the lock here, as required
        bucket.mutex.unlock();
        ParkResult::TimedOut
    })
}

/// Unparks one thread from the queue associated with the given key.
///
/// The `callback` function is called while the queue is locked and before the
/// target thread is woken up. The `UnparkResult` argument to the function
/// indicates whether a thread was found in the queue and whether this was the
/// last thread in the queue. This value is also returned by `unpark_one`.
///
/// The `callback` function should return an `UnparkToken` value which will be
/// passed to the thread that is unparked. If no thread is unparked then the
/// returned value is ignored.
///
/// # Safety
///
/// You should only call this function with an address that you control, since
/// you could otherwise interfere with the operation of other synchronization
/// primitives.
///
/// The `callback` function is called while the queue is locked and must not
/// panic or call into any function in `parking_lot`.
#[inline]
pub unsafe fn unpark_one(
    key: usize,
    callback: impl FnOnce(UnparkResult) -> UnparkToken,
) -> UnparkResult {
    // Lock the bucket for the given key
    let bucket = lock_bucket(key);

    // Find a thread with a matching key and remove it from the queue
    let mut link = &bucket.queue_head;
    let mut current = bucket.queue_head.get();
    let mut previous = ptr::null();
    let mut result = UnparkResult::default();
    while !current.is_null() {
        if (*current).key.load(Ordering::Relaxed) == key {
            // Remove the thread from the queue
            let next = (*current).next_in_queue.get();
            link.set(next);
            if bucket.queue_tail.get() == current {
                bucket.queue_tail.set(previous);
            } else {
                // Scan the rest of the queue to see if there are any other
                // entries with the given key.
                let mut scan = next;
                while !scan.is_null() {
                    if (*scan).key.load(Ordering::Relaxed) == key {
                        result.have_more_threads = true;
                        break;
                    }
                    scan = (*scan).next_in_queue.get();
                }
            }

            // Invoke the callback before waking up the thread
            result.unparked_threads = 1;
            result.be_fair = (*bucket.fair_timeout.get()).should_timeout();
            let token = callback(result);

            // Set the token for the target thread
            (*current).unpark_token.set(token);

            // This is a bit tricky: we first lock the ThreadParker to prevent
            // the thread from exiting and freeing its ThreadData if its wait
            // times out. Then we unlock the queue since we don't want to keep
            // the queue locked while we perform a system call. Finally we wake
            // up the parked thread.
            let handle = (*current).parker.unpark_lock();
            // SAFETY: We hold the lock here, as required
            bucket.mutex.unlock();
            handle.unpark();

            return result;
        } else {
            link = &(*current).next_in_queue;
            previous = current;
            current = link.get();
        }
    }

    // No threads with a matching key were found in the bucket
    callback(result);
    // SAFETY: We hold the lock here, as required
    bucket.mutex.unlock();
    result
}

mod deadlock_impl {
    use super::word_lock::WordLock;
    use super::{get_hashtable, lock_bucket, with_thread_data, ThreadData, NUM_THREADS};
    use super::{ThreadParkerT, UnparkHandleT};
    use backtrace::Backtrace;
    use std::cell::{Cell, UnsafeCell};
    use std::sync::mpsc;
    use thread_id;

    /// Representation of a deadlocked thread
    pub struct DeadlockedThread {
        thread_id: usize,
        backtrace: Backtrace,
    }

    impl DeadlockedThread {
        /// The system thread id
        pub fn thread_id(&self) -> usize {
            self.thread_id
        }

        /// The thread backtrace
        pub fn backtrace(&self) -> &Backtrace {
            &self.backtrace
        }
    }

    pub struct DeadlockData {
        // Currently owned resources (keys)
        resources: UnsafeCell<Vec<usize>>,

        // Set when there's a pending callstack request
        deadlocked: Cell<bool>,

        // Sender used to report the backtrace
        backtrace_sender: UnsafeCell<Option<mpsc::Sender<DeadlockedThread>>>,

        // System thread id
        thread_id: usize,
    }

    impl DeadlockData {
        pub fn new() -> Self {
            DeadlockData {
                resources: UnsafeCell::new(Vec::new()),
                deadlocked: Cell::new(false),
                backtrace_sender: UnsafeCell::new(None),
                thread_id: thread_id::get(),
            }
        }
    }

    pub(super) unsafe fn on_unpark(td: &ThreadData) {
        if td.deadlock_data.deadlocked.get() {
            let sender = (*td.deadlock_data.backtrace_sender.get()).take().unwrap();
            sender
                .send(DeadlockedThread {
                    thread_id: td.deadlock_data.thread_id,
                    backtrace: Backtrace::new(),
                })
                .unwrap();
            // make sure to close this sender
            drop(sender);

            // park until the end of the time
            td.parker.prepare_park();
            td.parker.park();
            unreachable!("unparked deadlocked thread!");
        }
    }

    pub unsafe fn acquire_resource(key: usize) {
        with_thread_data(|thread_data| {
            (*thread_data.deadlock_data.resources.get()).push(key);
        });
    }

    pub unsafe fn release_resource(key: usize) {
        with_thread_data(|thread_data| {
            let resources = &mut (*thread_data.deadlock_data.resources.get());

            // There is only one situation where we can fail to find the
            // resource: we are currently running TLS destructors and our
            // ThreadData has already been freed. There isn't much we can do
            // about it at this point, so just ignore it.
            if let Some(p) = resources.iter().rposition(|x| *x == key) {
                resources.swap_remove(p);
            }
        });
    }
}
