use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::task::{Context, Poll};

use futures_core::stream::Stream;

use crate::waker;
use crate::CoQueue;

#[macro_export]
macro_rules! pin_mut {
    ($x:ident) => {
        // Move the value to ensure that it is owned
        let mut $x = $x;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe { ::std::pin::Pin::new_unchecked(&mut $x) };
    };
    (&mut $x:ident) => {
        // Move the value to ensure that it is owned
        let $x = $x;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe { ::std::pin::Pin::new_unchecked(&mut **$x) };
    };
}

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

impl<T: fmt::Debug + Send + Sync> CoQueue<T> {
    fn yield_value(&mut self) -> impl Future<Output = QueueState<T>> + '_ {
        FutureStreamOwn(self)
    }

    pub fn into_iter(&mut self) -> CoQueIntoIter<T> {
        CoQueIntoIter { start: self }
    }

    pub fn sender(&self) -> Mutex<Sender<QueueState<T>>> {
        Mutex::new(self.send.clone())
    }
}

struct FutureStreamOwn<'a, T>(&'a mut CoQueue<T>);

impl<'a, T: fmt::Debug + Send + Sync> Future for FutureStreamOwn<'a, T> {
    type Output = QueueState<T>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = self.get_unchecked_mut();

            if !this.0.is_empty() {
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

impl<T: fmt::Debug + Send + Sync> Stream for CoQueue<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };
        match this.recv.try_recv() {
            Ok(QueueState::Terminate) => return Poll::Ready(None),
            Ok(QueueState::Error(err)) => panic!("{}", err),
            _ => {}
        }

        let fut = this.yield_value();
        pin_mut!(fut);

        match fut.poll(cx) {
            Poll::Ready(item) => match item {
                QueueState::Spin => return Poll::Pending,
                QueueState::Error(e) => panic!("{}", e),
                QueueState::Yield(item) => return Poll::Ready(Some(item)),
                _ => panic!("no other states possible BUG"),
            },
            _ => panic!("no other states possible BUG"),
        }
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

            let stream = &mut self.start;
            pin_mut!(&mut stream);

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

    use std::sync::RwLock;
    use std::thread;
    use std::time::Duration;

    use crossbeam::scope;
    use futures::StreamExt;

    #[test]
    fn stream_que() {
        let que = CoQueue::new();

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
                for item in thread_que.get_mut().unwrap().into_iter() {
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
