use std::{
    ptr,
    task::{RawWaker, RawWakerVTable, Waker},
};

/// Creates a vtable that has fn pointers to no-op functions
///
/// # Safety
/// Doing nothing is always a good safe thing to do, just sit and watch tv
/// don't go try something new and scary?!
pub fn create() -> Waker {
    unsafe { Waker::from_raw(RAW_WAKER) }
}

const RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &VTABLE);
const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

const unsafe fn clone(_: *const ()) -> RawWaker {
    RAW_WAKER
}

const unsafe fn wake(_: *const ()) {}

const unsafe fn wake_by_ref(_: *const ()) {}

const unsafe fn drop(_: *const ()) {}
