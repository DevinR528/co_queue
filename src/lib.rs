mod backoff;
mod cahch_pad;
mod queue;
mod stream;
mod waker;

pub use queue::CoQueue;

pub trait Message {
    type Terminate;
    type Error;
    type Item;

    fn is_shutdown(&self) -> bool;
}
