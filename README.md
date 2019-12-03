# Concurrent Queue

[![Build Status](https://travis-ci.com/DevinR528/spans.svg?branch=master)](https://travis-ci.com/DevinR528/co_queue)
[![Latest Version](https://img.shields.io/crates/v/spans.svg)](https://crates.io/crates/co_queue)

A rust nod to C#'s `ConcurrentQueue`. Provides an infinite streaming iterator that can be pushed to 
from multiple threads a mpsc queue based on crossbeam's `SegQueue`.
<br>
Exploration of possible PR to crossbeam or an extension trait crate.

## Use
```toml
co_queue = "0.1"
```
`CoQueue` is a simple collection that you push "jobs" into and the `CoQueue` handles
spawning threads and communicating with the main thread.

## Examples
Handle expensive tasks on a separate thread as an iterator or stream.
### Iterator
```rust
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
            println!("{:?}", thread_que.read().unwrap().len());
        });
    })
    .unwrap();

    scope(|scope| {
        scope.spawn(|_| {
            
            for (i, item) in thread_que.get_mut().unwrap().into_iter().enumerate() {
                x += 1;
                thread::sleep(Duration::from_millis(100));
                if let IterWaker::Item(i) = item {
                    println!("item {}", i)
                }
                if x > 11 {
                    sender.lock().unwrap().send(QueueState::Terminate).unwrap();
                }
            }
        });
    })
    .unwrap();
}
```
### Stream
```rust
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

    }).unwrap();
    scope(|scope| {
        scope.spawn(|_| {
            // thread::sleep(Duration::from_millis(100));
            for _ in 0..5  {
                futures::executor::block_on(async {
                    let res = thread_que.get_mut().unwrap().next().await;
                    println!("{:?}", res);
                });
            }
        });
    }).unwrap();
}
```
more docs to come!

## Contribute
[Pull requests](https://github.com/DevinR528/co_queue/pulls) or [suggestions](https://github.com/DevinR528/co_queue/issues) welcome!

#### License
<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>
<br>
<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
</sub>
