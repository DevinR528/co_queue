use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvError, SendError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::thread;

use futures_core::stream::Stream;

use crate::backoff::Backoff;
use crate::cahch_pad::CachePad;
use crate::waker;
use crate::CoQueue;

#[derive(Debug)]
pub enum QueueError {
    Error,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for QueueError {}

#[derive(Debug)]
pub enum QueueState<T> {
    Error(QueueError),
    Yield(T),
    Spin,
    Terminate,
}

struct FutureStreamOwn<'a, T>(&'a mut CoQueue<T>);

impl<'a, T: fmt::Debug + Send + Sync> Future for FutureStreamOwn<'a, T> {
    type Output = QueueState<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = self.get_unchecked_mut();

            if this.0.len() != 0 {
                if let Ok(item) = this.0.pop() {
                    Poll::Ready(QueueState::Yield(item))
                } else {
                    Poll::Ready(QueueState::Error(QueueError::Error))
                }
            } else {
                Poll::Ready(QueueState::Spin)
            }
        }
    }
}

impl<T: fmt::Debug + Send + Sync> CoQueue<T> {
    fn yield_value(&mut self) -> impl Future<Output = QueueState<T>> + '_ {
        FutureStreamOwn(self)
    }

    pub fn iter(&self) -> CoQueueIter<'_, T> {
        CoQueueIter {
            start: self,
            item: None,
        }
    }
    pub fn into_iter(&mut self) -> CoQueIntoIter<T> {
        CoQueIntoIter { start: self }
    }
    pub fn sender(&self) -> Mutex<Sender<QueueState<T>>> {
        Mutex::new(self.temp_tx.clone())
    }
}

impl<T: fmt::Debug + Send + Sync> Stream for CoQueue<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unsafe {
            let this = self.get_unchecked_mut();
            match this.recv.try_recv() {
                Ok(QueueState::Terminate) => return Poll::Ready(None),
                Ok(QueueState::Error(err)) => panic!("{}", err),
                _ => {}
            }

            let mut fut = this.yield_value();
            let fut = Pin::new_unchecked(&mut fut);

            match fut.poll(cx) {
                Poll::Ready(item) => match item {
                    QueueState::Spin => return Poll::Pending,
                    QueueState::Error(e) => panic!("{}", e),
                    QueueState::Yield(item) => return Poll::Ready(Some(item)),
                    _ => panic!("no other states possible BUG"),
                },
                _ => panic!("no other states possible BUG"),
            }
        };
    }
}

#[derive(Debug)]
pub enum IterWaker<T> {
    Item(T),
    Poke,
}

pub struct CoQueIntoIter<'a, T> {
    start: &'a mut CoQueue<T>,
}

impl<'a, T: fmt::Debug + Send + Sync> Iterator for CoQueIntoIter<'a, T> {
    type Item = IterWaker<T>;
    fn next(&mut self) -> Option<Self::Item> {
        let waker = waker::create();
        let mut cx = Context::from_waker(&waker);

        let mut wake_count = 0;
        loop {
            match self.start.recv.try_recv() {
                Ok(QueueState::Terminate) => return None,
                Ok(QueueState::Error(err)) => panic!("{}", err),
                _ => {}
            }
            let stream = unsafe { Pin::new_unchecked(&mut *self.start) };
            match stream.poll_next(&mut cx) {
                Poll::Ready(Some(res)) => return Some(IterWaker::Item(res)),
                Poll::Ready(None) => return None,
                Poll::Pending => {
                    if wake_count == 20 {
                        return Some(IterWaker::Poke);
                    }
                    wake_count += 1;
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    use crossbeam::scope;
    use futures::StreamExt;
    use std::time::Duration;

    #[test]
    fn stream_que() {
        let mut x = 0;

        let que = CoQueue::new();
        let sender = que.sender();

        let mut thread_que: RwLock<CoQueue<u8>> = RwLock::new(que);

        scope(|scope| {
            scope.spawn(|_| {
                // thread::sleep(Duration::from_millis(100));
                for i in 0..5 {
                    thread_que.write().unwrap().push(i);
                }
            });
        })
        .unwrap();
        scope(|scope| {
            scope.spawn(|_| {
                // thread::sleep(Duration::from_millis(100));
                for i in 0..5 {
                    futures::executor::block_on(async {
                        let res = thread_que.get_mut().unwrap().next().await;
                        assert_eq!(Some(i), res);
                    });
                }
            });
        })
        .unwrap();
    }

    #[test]
    fn streaming_iter_que() {
        let mut x = 0;

        let que = CoQueue::new();
        let sender = que.sender();

        let mut thread_que: RwLock<CoQueue<u8>> = RwLock::new(que);

        scope(|scope| {
            scope.spawn(|_| {
                for i in 0..5 {
                    thread::sleep(Duration::from_millis(100));
                    thread_que.write().unwrap().push(i);
                }

                for i in 0..5 {
                    thread::sleep(Duration::from_millis(300));
                    thread_que.write().unwrap().push(i);
                }
                assert_eq!(10, thread_que.read().unwrap().len());
            });
        })
        .unwrap();

        scope(|scope| {
            scope.spawn(|_| {
                for (i, item) in thread_que.get_mut().unwrap().into_iter().enumerate() {
                    thread::sleep(Duration::from_millis(100));
                    if let IterWaker::Item(i) = item {
                        assert!(i <= 4);
                    }

                    x += 1;
                    if x > 11 {
                        sender.lock().unwrap().send(QueueState::Terminate).unwrap();
                    }
                }
                // make sure it does not hang forever
                assert!(thread_que.read().unwrap().is_empty())
            });
        })
        .unwrap();
    }
}
