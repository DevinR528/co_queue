# Concurrent Queue

[![Build Status](https://travis-ci.com/DevinR528/spans.svg?branch=master)](https://travis-ci.com/DevinR528/co_queue)
[![Latest Version](https://img.shields.io/crates/v/spans.svg)](https://crates.io/crates/co_queue)

A rust implementation of C#'s `Span<T>` and `Memory<T>`. Provides zero copy slicing, indexing,
and iteration of any contiguous memory type (arrays, slices, vecs, ect).

## Use
```toml
co_queue = "0.1"
```
`CoQueue` is a simple collection that you push "jobs" into and the `CoQueue` handles
spawning threads and communicating with the main thread.

## Examples
Handle expensive tasks on a separate thread. 
```rust
use std::thread;
use co_queue::CoQueue;
use std::time::Duration;

let mut x = 0;

let mut que = CoQueue::new();
let rx = que.listener();

let job = || {
    thread::sleep(Duration::from_millis(100));
    x += 1;
    println!("{}", x)
};

for i in 0..5 {
    que.push(job);
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
