use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Mutex, RwLock,
};
use std::task::{Context, Poll};

use futures_core::stream::Stream;

use crate::stream::{IterWaker, QueueError, QueueState};
use crate::waker;
use crate::{pin_mut, CoQueue};

pub struct BlockingQueue<T> {
    que: RwLock<CoQueue<T>>,
    recv: Receiver<QueueState<T>>,
    send: Sender<QueueState<T>>,
}

unsafe impl<T: Send> Send for BlockingQueue<T> {}
unsafe impl<T: Sync> Sync for BlockingQueue<T> {}

impl<T> BlockingQueue<T>
where
    T: fmt::Debug + Send + Sync,
{
    pub fn new() -> BlockingQueue<T> {
        let (send, recv) = channel();
        Self {
            que: RwLock::new(CoQueue::new()),
            recv,
            send,
        }
    }

    pub fn push(&self, item: T) {
        self.que.read().unwrap().push(item)
    }

    pub fn pop(&self) -> Result<T, ()>
    where
        T: std::fmt::Debug,
    {
        self.que.read().unwrap().pop()
    }

    pub fn is_empty(&self) -> bool {
        self.que.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.que.read().unwrap().len()
    }

    // fn yield_value(&self) -> impl Future<Output = QueueState<T>> + '_ {
    //     FutureStreamOwn(self)
    // }

    pub fn iter(&self) -> BlockingIter<T> {
        BlockingIter(self)
    }

    pub fn sender(&self) -> Mutex<Sender<QueueState<T>>> {
        Mutex::new(self.send.clone())
    }
}

// struct FutureStreamOwn<'a, T>(&'a BlockingQueue<T>);

// impl<'a, T: fmt::Debug + Send + Sync> Future for FutureStreamOwn<'a, T> {
//     type Output = QueueState<T>;
//     fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
//         unsafe {
//             let this = self.get_unchecked_mut();

//             if this.0.len() != 0 {
//                 if let Ok(item) = this.0.pop() {
//                     Poll::Ready(QueueState::Yield(item))
//                 } else {
//                     Poll::Ready(QueueState::Error(QueueError::Error))
//                 }
//             } else {
//                 Poll::Ready(QueueState::Spin)
//             }
//         }
//     }
// }

// impl<T: fmt::Debug + Send + Sync> Stream for BlockingQueue<T> {
//     type Item = T;
//     fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {

//         let this = unsafe { self.get_unchecked_mut() };
//         match this.recv.try_recv() {
//             Ok(QueueState::Terminate) => return Poll::Ready(None),
//             Ok(QueueState::Error(err)) => panic!("{}", err),
//             _ => {}
//         }

//         let fut = this.yield_value();
//         pin_mut!(fut);

//         match fut.poll(cx) {
//             Poll::Ready(item) => match item {
//                 QueueState::Spin => return Poll::Pending,
//                 QueueState::Error(e) => panic!("{}", e),
//                 QueueState::Yield(item) => return Poll::Ready(Some(item)),
//                 _ => panic!("no other states possible BUG"),
//             },
//             _ => panic!("no other states possible BUG"),
//         }
//     }
// }

pub struct BlockingIter<'a, T>(&'a BlockingQueue<T>);

impl<'a, T: fmt::Debug + Send + Sync> Iterator for BlockingIter<'a, T> {
    type Item = IterWaker<T>;
    fn next(&mut self) -> Option<Self::Item> {
        let waker = waker::create();
        let mut cx = Context::from_waker(&waker);

        let mut wake_count = 0;
        loop {
            match self.0.recv.try_recv() {
                Ok(QueueState::Terminate) => return None,
                Ok(QueueState::Error(err)) => panic!("{}", err),
                _ => {}
            }

            let mut que = self.0.que.write().unwrap();
            let stream = unsafe { Pin::new_unchecked(&mut *que) };

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
    use std::thread;
    use std::time::Duration;

    use crossbeam::scope;
    use futures::StreamExt;

    #[test]
    fn non_mut_iter_que() {
        let mut x = 0;

        let que = BlockingQueue::new();
        let sender = que.sender();

        scope(|scope| {
            scope.spawn(|_| {
                for i in 0..5 {
                    que.push(i);
                }

                for i in 0..5 {
                    que.push(i);
                }
                assert_eq!(10, que.len());
            });
        })
        .unwrap();

        scope(|scope| {
            scope.spawn(|_| {
                for item in que.iter() {
                    if let IterWaker::Item(i) = item {
                        assert!(i <= 4);
                    }

                    x += 1;
                    if x > 11 {
                        sender.lock().unwrap().send(QueueState::Terminate).unwrap();
                    }
                }
                // make sure it does not hang forever
                assert!(que.is_empty())
            });
        })
        .unwrap();
    }
}
