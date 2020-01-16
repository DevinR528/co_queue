#![feature(const_fn)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_mut)]
#![allow(non_snake_case)]

// mod backoff;
// mod blocking_col;
// mod cahch_pad;
// mod lockless_queue;
// mod stream;
// mod waker;

// pub use lockless_queue::CoQueue;

mod mutex;
/// start of actual possible crate
mod queue;

pub use mutex::{MapMootexGuard, Mootex, MootexGuard};
pub use queue::ShiftQueue;
