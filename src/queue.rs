use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvError, SendError, Sender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;

use futures_core::stream::Stream;

use crate::backoff::Backoff;
use crate::cahch_pad::CachePad;
use crate::stream::QueueState;

// Bits indicating the state of a slot:
// * If a value has been written into the slot, `WRITE` is set.
// * If a value has been read from the slot, `READ` is set.
// * If the block is being destroyed, `DESTROY` is set.
const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;
// each block is LAP number of indexes
const LAP: usize = 32;
// max number of values chunk holds
const CHUNK_CAP: usize = LAP - 1;
// How many lower bits are reserved for metadata.
const SHIFT: usize = 1;
// Indicates that the block is not the last one.
const HAS_NEXT: usize = 1;

#[derive(Debug)]
struct Slot<T> {
    value: UnsafeCell<ManuallyDrop<T>>,
    state: AtomicUsize,
}

impl<T> Slot<T> {
    fn wait_write(&self) {
        let backoff = Backoff::new();
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            backoff.nap();
        }
    }
}
#[derive(Debug)]
pub struct Chunk<T> {
    next: AtomicPtr<Chunk<T>>,
    /// replace for CHUNK_CAP
    slots: [Slot<T>; CHUNK_CAP],
}

/// this is block
impl<T> Chunk<T> {
    fn new() -> Chunk<T> {
        unsafe { mem::zeroed() }
    }

    fn wait_next(&self) -> *mut Chunk<T> {
        let backoff = Backoff::new();

        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            backoff.nap();
        }
    }

    unsafe fn destroy(this: *mut Chunk<T>, start: usize) {
        for i in start..CHUNK_CAP {
            let slot = (*this).slots.get_unchecked(i);

            if slot.state.load(Ordering::Acquire) & READ == 0
                && slot.state.fetch_or(DESTROY, Ordering::AcqRel) & READ == 0
            {
                println!("no drop still in use");
                return;
            }
        }
        println!("drop not in use");
        drop(Box::from_raw(this))
    }
}

/// this is Position equiv
#[derive(Debug)]
pub struct Segment<T> {
    index: AtomicUsize,
    chunk: AtomicPtr<Chunk<T>>,
}

impl<T> Segment<T> {
    fn new() -> Segment<T> {
        Self {
            index: AtomicUsize::new(0),
            chunk: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// TODO ordering will be important
/// have a listener method to get mpsc::Reciver
pub struct CoQueue<T> {
    head: CachePad<Segment<T>>,
    tail: CachePad<Segment<T>>,
    pub(crate) recv: Receiver<QueueState<T>>,
    pub(crate) temp_tx: Sender<QueueState<T>>,
    _mkr: PhantomData<T>,
}

unsafe impl<T: Send> Send for CoQueue<T> {}
unsafe impl<T: Sync> Sync for CoQueue<T> {}

impl<T> CoQueue<T> {
    pub fn new() -> CoQueue<T> {
        let (tx, recv) = channel();
        Self {
            head: CachePad::new(Segment {
                chunk: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            tail: CachePad::new(Segment {
                chunk: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            }),
            recv,
            temp_tx: tx,
            _mkr: PhantomData,
        }
    }

    pub fn push(&self, item: T) {
        let backoff = Backoff::new();
        let mut tail = self.tail.index.load(Ordering::Acquire);
        let mut chunk = self.tail.chunk.load(Ordering::Acquire);
        let mut next_chunk = None;

        loop {
            let offset = (tail >> SHIFT) % LAP;

            if offset == CHUNK_CAP {
                backoff.nap();
                tail = self.tail.index.load(Ordering::Acquire);
                chunk = self.tail.chunk.load(Ordering::Acquire);
                continue;
            }

            if offset + 1 == CHUNK_CAP && next_chunk.is_none() {
                next_chunk = Some(Box::new(Chunk::<T>::new()));
            }

            if chunk.is_null() {
                let new = Box::into_raw(Box::new(Chunk::<T>::new()));

                if self
                    .tail
                    .chunk
                    .compare_and_swap(chunk, new, Ordering::Release)
                    == chunk
                {
                    self.head.chunk.store(new, Ordering::Release);
                    chunk = new;
                } else {
                    next_chunk = unsafe { Some(Box::from_raw(new)) };
                    tail = self.tail.index.load(Ordering::Acquire);
                    chunk = self.tail.chunk.load(Ordering::Acquire);
                    continue;
                }
            }
            let new_tail = tail + (1 << SHIFT);

            // move tail forward
            match self.tail.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    if offset + 1 == CHUNK_CAP {
                        let next_chunk = Box::into_raw(next_chunk.unwrap());
                        let next_idx = new_tail.wrapping_add(1 << SHIFT);

                        self.tail.chunk.store(next_chunk, Ordering::Release);
                        self.tail.index.store(next_idx, Ordering::Release);
                        (*chunk).next.store(next_chunk, Ordering::Release);
                    }

                    let slot = (*chunk).slots.get_unchecked(offset);
                    slot.value.get().write(ManuallyDrop::new(item));
                    slot.state.fetch_or(WRITE, Ordering::Release);

                    return;
                },
                Err(t) => {
                    tail = t;
                    chunk = self.tail.chunk.load(Ordering::Acquire);
                    backoff.spin();
                }
            }
        }
    }

    pub fn pop(&self) -> Result<T, ()>
    where
        T: std::fmt::Debug,
    {
        let backoff = Backoff::new();
        let mut head = self.head.index.load(Ordering::Acquire);
        let mut chunk = self.head.chunk.load(Ordering::Acquire);

        loop {
            // println!("HEAD {}", head);
            // head left shift 1 mod 32
            let offset = (head >> SHIFT) % LAP;
            if offset == CHUNK_CAP {
                backoff.nap();
                head = self.head.index.load(Ordering::Acquire);
                chunk = self.head.chunk.load(Ordering::Acquire);
                continue;
            }

            let mut new_head = head + (1 << SHIFT);
            //println!("new head {}", new_head);
            if new_head & HAS_NEXT == 0 {
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Relaxed);

                // If the tail equals the head, that means the queue is empty.
                if head >> SHIFT == tail >> SHIFT {
                    return Err(());
                }

                // If head and tail are not in the same block, set `HAS_NEXT` in head.
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= HAS_NEXT;
                }
            }

            if chunk.is_null() {
                backoff.nap();
                head = self.head.index.load(Ordering::Acquire);
                chunk = self.head.chunk.load(Ordering::Acquire);
                continue;
            }
            // move head forward
            match self.head.index.compare_exchange_weak(
                head,
                new_head,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    if offset + 1 == CHUNK_CAP {
                        let next = (*chunk).wait_next();
                        let mut next_idx = (new_head & !HAS_NEXT).wrapping_add(1 << SHIFT);
                        if !(*next).next.load(Ordering::Relaxed).is_null() {
                            next_idx |= HAS_NEXT;
                        }

                        self.head.chunk.store(next, Ordering::Release);
                        self.head.index.store(next_idx, Ordering::Release);
                    }

                    let slot = (*chunk).slots.get_unchecked(offset);
                    slot.wait_write();
                    let m = slot.value.get().read();
                    let item = ManuallyDrop::into_inner(m);

                    // Destroy the chunk if we've reached the end, or if another thread wanted to
                    // destroy but couldn't because we were busy reading from the slot.
                    if offset + 1 == CHUNK_CAP {
                        Chunk::destroy(chunk, 0);
                    } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
                        Chunk::destroy(chunk, offset + 1);
                    }
                    // println!("pop return {:?}", item);
                    return Ok(item);
                },
                Err(h) => {
                    head = h;
                    chunk = self.head.chunk.load(Ordering::Acquire);
                    backoff.spin();
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }

    pub fn len(&self) -> usize {
        loop {
            // Load the tail index, then load the head index.
            let mut tail = self.tail.index.load(Ordering::SeqCst);
            let mut head = self.head.index.load(Ordering::SeqCst);

            // If the tail index didn't change, we've got consistent indices to work with.
            if self.tail.index.load(Ordering::SeqCst) == tail {
                // Erase the lower bits.
                tail &= !((1 << SHIFT) - 1);
                head &= !((1 << SHIFT) - 1);

                // Rotate indices so that head falls into the first block.
                let lap = (head >> SHIFT) / LAP;
                tail = tail.wrapping_sub((lap * LAP) << SHIFT);
                head = head.wrapping_sub((lap * LAP) << SHIFT);

                // Remove the lower bits.
                tail >>= SHIFT;
                head >>= SHIFT;

                // Fix up indices if they fall onto block ends.
                if head == CHUNK_CAP {
                    head = 0;
                    tail -= LAP;
                }
                if tail == CHUNK_CAP {
                    tail += 1;
                }

                // Return the difference minus the number of blocks between tail and head.
                return tail - head - tail / LAP;
            }
        }
    }
}

impl<T> fmt::Debug for CoQueue<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // f.debug_list().entries(self.iter()).finish()
        f.debug_list().entry(&self.head).entry(&self.tail).finish()
    }
}

impl<T> Drop for CoQueue<T> {
    fn drop(&mut self) {
        let mut head = self.head.index.load(Ordering::Relaxed);
        let mut tail = self.tail.index.load(Ordering::Relaxed);
        let mut chunk = self.head.chunk.load(Ordering::Relaxed);

        // Erase the lower bits.
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all values between `head` and `tail` and deallocate the heap-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;

                if offset < CHUNK_CAP {
                    // Drop the value in the slot.
                    let slot = (*chunk).slots.get_unchecked(offset);
                    ManuallyDrop::drop(&mut *(*slot).value.get());
                } else {
                    // Deallocate the chunk and move to the next one.
                    let next = (*chunk).next.load(Ordering::Relaxed);
                    drop(Box::from_raw(chunk));
                    chunk = next;
                }

                head = head.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining chunk.
            if !chunk.is_null() {
                drop(Box::from_raw(chunk));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::scope;
    use std::time::Duration;

    #[test]
    fn spsc() {
        const COUNT: usize = 10;

        let q = CoQueue::new();

        scope(|scope| {
            scope.spawn(|_| {
                for i in 0..=COUNT {
                    loop {
                        if let Ok(x) = q.pop() {
                            assert_eq!(x, i);
                            break;
                        }
                    }
                }
                assert!(q.pop().is_err());
            });
            scope.spawn(|_| {
                for i in 0..=COUNT {
                    q.push(i);
                }
                println!("DONE");
            });
        })
        .unwrap();
    }

    #[test]
    fn mpmc() {
        const COUNT: usize = 20;
        const THREADS: usize = 4;

        let q = CoQueue::<usize>::new();
        let v = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();

        scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|_| {
                    for _ in 0..COUNT {
                        let n = loop {
                            if let Ok(x) = q.pop() {
                                break x;
                            }
                        };
                        v[n].fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
            for _ in 0..THREADS {
                scope.spawn(|_| {
                    for i in 0..COUNT {
                        q.push(i);
                    }
                });
            }
        })
        .unwrap();

        for c in v {
            assert_eq!(c.load(Ordering::SeqCst), THREADS);
        }
    }

    #[test]
    fn ordering() {
        let arr: &[AtomicPtr<u8>; 5] = &[
            AtomicPtr::new(ptr::null_mut()),
            AtomicPtr::new(ptr::null_mut()),
            AtomicPtr::new(ptr::null_mut()),
            AtomicPtr::new(ptr::null_mut()),
            AtomicPtr::new(ptr::null_mut()),
        ];

        // while arr[0].load(Ordering::Acquire).is_null() {
        //     for i in 0..5 {
        //         unsafe {
        //             let y = arr[i].load(Ordering::Acquire);
        //             println!("outside {:?}", y.as_ref());
        //         }
        //     }
        // }
        let _ = scope(|scope| {
            scope.spawn(move |_| unsafe {
                for i in 0..5 {
                    let new = Box::into_raw(Box::new(i as u8));
                    arr[i].store(new, Ordering::Release);
                }
                for i in 0..5 {
                    thread::sleep_ms(100);
                    let y = arr[i].load(Ordering::Acquire);
                    println!("{:?}", y.as_ref());
                }
            });
        })
        .unwrap();
    }
}
