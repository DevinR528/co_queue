use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

//use super::deadlock::{acquire_resource, release_resource};
use super::deadlock::{self, ParkResult, UnparkResult, UnparkToken, DEFAULT_PARK_TOKEN};
use super::mutex::Mutexable;

// UnparkToken used to indicate that that the target thread should attempt to
// lock the mutex again as soon as it is unparked.
pub(crate) const TOKEN_NORMAL: UnparkToken = UnparkToken(0);

// UnparkToken used to indicate that the mutex is being handed off to the target
// thread directly without unlocking it.
pub(crate) const TOKEN_HANDOFF: UnparkToken = UnparkToken(1);

/// This bit is set in the `state` of a `RawMutex` when that mutex is locked by some thread.
const LOCKED_BIT: u8 = 0b01;
/// This bit is set in the `state` of a `RawMutex` just before parking a thread. A thread is being
/// parked if it wants to lock the mutex, but it is currently being held by some other thread.
const PARKED_BIT: u8 = 0b10;

#[derive(Debug)]
/// Raw mutex type backed by the parking lot.
pub struct RawMutex {
    /// This atomic integer holds the current state of the mutex instance. Only the two lowest bits
    /// are used. See `LOCKED_BIT` and `PARKED_BIT` for the bitmask for these bits.
    ///
    /// # State table:
    ///
    /// PARKED_BIT | LOCKED_BIT | Description
    ///     0      |     0      | The mutex is not locked, nor is anyone waiting for it.
    /// -----------+------------+------------------------------------------------------------------
    ///     0      |     1      | The mutex is locked by exactly one thread. No other thread is
    ///            |            | waiting for it.
    /// -----------+------------+------------------------------------------------------------------
    ///     1      |     0      | The mutex is not locked. One or more thread is parked or about to
    ///            |            | park. At least one of the parked threads are just about to be
    ///            |            | unparked, or a thread heading for parking might abort the park.
    /// -----------+------------+------------------------------------------------------------------
    ///     1      |     1      | The mutex is locked by exactly one thread. One or more thread is
    ///            |            | parked or about to park, waiting for the lock to become available.
    ///            |            | In this state, PARKED_BIT is only ever cleared when a bucket lock
    ///            |            | is held (i.e. in a parking_lot_core callback). This ensures that
    ///            |            | we never end up in a situation where there are parked threads but
    ///            |            | PARKED_BIT is not set (which would result in those threads
    ///            |            | potentially never getting woken up).
    state: AtomicU8,
}

impl RawMutex {
    pub(crate) fn mark_parked_when_lock(&self) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & LOCKED_BIT == 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | PARKED_BIT,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(x) => state = x,
            }
        }
    }

    pub(crate) fn lock_slow(&self) -> bool {
        let mut wait = deadlock::SpinWait::new();
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & LOCKED_BIT == 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state | LOCKED_BIT,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(x) => state = x,
                }
                continue;
            }

            // if no queue try spinning before checking again
            if state & PARKED_BIT == 0 && wait.spin() {
                state = self.state.load(Ordering::Relaxed);
                continue;
            }

            // Set the parked bit
            if state & PARKED_BIT == 0 {
                if let Err(x) = self.state.compare_exchange_weak(
                    state,
                    state | PARKED_BIT,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    state = x;
                    continue;
                }
            }

            // park our thread until unlock
            let addr = self as *const _ as usize;
            let validate = || self.state.load(Ordering::Relaxed) == LOCKED_BIT | PARKED_BIT;
            let before_sleep = || {};
            let time_out = |_, was_last_thread| {
                if was_last_thread {
                    self.state.fetch_and(!PARKED_BIT, Ordering::Relaxed);
                }
            };

            match unsafe {
                deadlock::park(addr, validate, || {}, time_out, DEFAULT_PARK_TOKEN, None)
            } {
                // The thread that unparked us passed the lock on to us
                // directly without unlocking it.
                ParkResult::Unparked(TOKEN_HANDOFF) => return true,
                // We were unparked normally, try acquiring the lock again
                ParkResult::Unparked(_) => (),
                // The validation function failed, try locking again
                ParkResult::Invalid => (),
                // Timeout expired
                ParkResult::TimedOut => return false,
            }

            wait.spin();
            state = self.state.load(Ordering::Relaxed);
        }
    }

    pub(crate) fn unlock_slow(&self) {
        let addr = self as *const _ as usize;
        let cb = |res: UnparkResult| {
            if res.unparked_threads != 0 && res.be_fair {
                if !res.have_more_threads {
                    self.state.store(LOCKED_BIT, Ordering::Relaxed);
                }
                return TOKEN_HANDOFF;
            }

            if res.have_more_threads {
                self.state.store(PARKED_BIT, Ordering::Release);
            } else {
                self.state.store(0, Ordering::Release);
            }
            TOKEN_NORMAL
        };

        unsafe {
            deadlock::unpark_one(addr, cb);
        }
    }
}

unsafe impl Mutexable for RawMutex {
    const INIT: RawMutex = RawMutex {
        state: AtomicU8::new(0),
    };

    #[inline]
    fn lock(&self) {
        if self
            .state
            .compare_exchange_weak(0, LOCKED_BIT, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_slow();
        }
        unsafe { deadlock::acquire_resource(self as *const _ as usize) }
    }

    #[inline]
    fn try_lock(&self) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & LOCKED_BIT != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | PARKED_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    unsafe { deadlock::acquire_resource(self as *const _ as usize) };
                    return true;
                }
                Err(x) => state = x,
            }
        }
    }

    #[inline]
    fn unlock(&self) {
        unsafe { deadlock::release_resource(self as *const _ as usize) };
        if self
            .state
            .compare_exchange(LOCKED_BIT, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.unlock_slow();
    }
}
