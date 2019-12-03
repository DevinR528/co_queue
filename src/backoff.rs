use core::cell::Cell;
use core::fmt;
use core::sync::atomic;

const SPIN_LIMIT: u32 = 6;
const YIELD_LIMIT: u32 = 10;

pub struct Backoff {
    step: Cell<u32>,
}

impl Backoff {
    pub fn new() -> Backoff {
        Self { step: Cell::new(0) }
    }

    #[allow(dead_code)]
    pub fn reset(&self) {
        self.step.set(0)
    }

    pub fn spin(&self) {
        // println!("spin for {}", 1 << self.step.get().min(SPIN_LIMIT));
        for _ in 0..1 << self.step.get().min(SPIN_LIMIT) {
            atomic::spin_loop_hint();
        }
        if self.step.get() <= SPIN_LIMIT {
            self.step.set(self.step.get() + 1)
        }
    }

    pub fn nap(&self) {
        if self.step.get() <= SPIN_LIMIT {
            for _ in 0..1 << self.step.get() {
                atomic::spin_loop_hint();
            }
        } else {
            // println!("yield now");
            std::sync::Condvar::new();
            ::std::thread::yield_now();
        }

        if self.step.get() <= YIELD_LIMIT {
            self.step.set(self.step.get() + 1)
        }
    }

    pub fn is_completed(&self) -> bool {
        self.step.get() > YIELD_LIMIT
    }
}
impl fmt::Debug for Backoff {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Backoff")
            .field("step", &self.step)
            .field("is_completed", &self.is_completed())
            .finish()
    }
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff::new()
    }
}
