# histogram-3faa

This repository is an experimental implementation of an optimized [Prometheus histogram](https://prometheus.io/docs/concepts/metric_types/#histogram). The algorithm achieves the following key properties:

- **Lock-Free Critical Path**: Requires only **three** atomic fetch-and-add operations per observation.
- **Cache Locality**: Groups all atomic counters within the same cache line.

## Table of Contents
- [State of the Art](#state-of-the-art)
- [Algorithm](#algorithm)
  - [Reading a Consistent State](#reading-a-consistent-state)
  - [Breaking the Reading Loop](#breaking-the-reading-loop)
    - [Observation Order Inversion Edge Case](#observation-order-inversion-edge-case)
  - [Avoiding Unbounded Spinning](#avoiding-unbounded-spinning)
  - [Cache Locality](#cache-locality)
  - [Testing](#testing)
  - [NaN Support](#nan-support)
- [Disclaimer](#disclaimer)

## State of the Art

A histogram consists of multiple counters that must be incremented to ensure consistent metric collection, as if all counters were updated in a single atomic operation.

The simplest approach is to protect all counters with a mutex, as implemented in the current Rust Prometheus client. However, mutexes suffer significant performance degradation under contention. While collection performance is less critical due to Prometheus’s low-frequency scraping interval, the `observe` operation is a critical path, as metrics should minimally impact instrumented code. Thus, a lock-free alternative is highly desirable.

Contention between observations can be reduced by combining a read-write mutex with atomic counter increments, as observations acquire the read lock and execute in parallel. However, this approach still allows collection to interfere with observations.

In contrast, early versions of the Go client and the unofficial Rust TiKV client relied solely on atomic operations without synchronization, leading to potentially inconsistent collections. A significant improvement was introduced in [prometheus/client_golang#457](https://github.com/prometheus/client_golang/pull/457), which ensures consistency using a hot/cold shard mechanism.
<br>
Histogram counters are duplicated across a “hot” and a “cold” shard. An observation counter, incremented before each observation, includes a bit flag to determine the hot shard. During collection, the bit flag is flipped to switch shards, and the previously hot shard’s `_count` is compared to the observation count before the flip in a loop. When these counts match, no further observations occur on the newly cold shard, enabling consistent collection.

The current Go implementation, also adopted by the Rust TiKV client, represents the state of the art. It achieves lock-free observations with consistent collections. Since atomic [read-modify-write](https://en.wikipedia.org/wiki/Read%E2%80%93modify%E2%80%93write) operations, such as [fetch-and-add](https://en.wikipedia.org/wiki/Fetch-and-add), are relatively expensive, their count is a key indicator of efficiency in lock-free algorithms. Excluding retries for incrementing the float counter[^1], this approach requires **four** fetch-and-add operations.

It’s worth noting that both the Go and Rust TiKV implementations use spin loops for collection instead of waiting primitives — TiKV client doesn’t even use `std::hint::spin_loop`! Unbounded spin loops are [considered harmful](https://matklad.github.io/2020/01/02/spinlocks-considered-harmful.html), and `runtime.Gosched` introduces issues similar to those described for [`sched_yield` by Linus Torvalds](https://www.realworldtech.com/forum/?threadid=189711&curpostid=189752)[^2].

In addition to using a proper waiting mechanism, the algorithm presented here requires only **three** fetch-and-add operations for observation, one fewer than the Go implementation.

## Algorithm

### Reading a Consistent State

The algorithm stems from the question: Can the redundancy between the `_count` counter and the bucket counters ensure consistent reads, including `_sum`? The answer is yes, using the following sequence:

- **Observation**:
  - Increment the bucket counter (*relaxed* ordering).
  - Increment the `_sum` counter (*release* ordering).
  - Increment the `_count` counter (*release* ordering).
- **Collection**:
  - Read the `_count` counter (*acquire* ordering).
  - Read the `_sum` counter (*acquire* ordering).
  - Read and sum all bucket counters (*relaxed* ordering).
  - Compare the bucket sum to `_count`; return if they match, otherwise repeat.

*Release* ordering ensures observation increments are not reordered and synchronize with *acquire* reads during collection. Since `_count` is read first, `_sum` and bucket counters are at least as recent as `_count`. If bucket counters include a more recent observation, their sum will not match `_count`, triggering a loop. Similarly, if `_sum` includes a more recent observation, bucket counters will also be more recent due to ordering, causing a loop.

### Breaking the Reading Loop

While the sequence ensures consistent reads, continuous observations may cause collection to loop indefinitely. The solution also uses sharding: observations occur on one shard, and collection switches to the second shard to read the first. A mutex prevents concurrent collections.

Unlike the Go client’s algorithm, this approach does not wait for all observations to complete on a shard; it ensures only reading consistency. Thus, resetting the read shard and merging its content into the other is not feasible. Instead, both shards are read separately, and their results are summed, splitting the histogram state across shards rather than replicating it.

#### Observation Order Inversion Edge Case

Since collection does not wait for all observations to complete on a shard before reading the second, an observation may complete on the first shard after its collection, while a subsequent observation is counted in the second shard’s collection. This edge case is acceptable for metrics, as the first observation will be included in the next collection.

### Avoiding Unbounded Spinning

As noted in the [State of the Art](#state-of-the-art) section, unbounded spin loops are problematic. If a collection is blocked by an incomplete observation, the algorithm uses a proper waiting mechanism after a brief spin loop. The implementation employs `futures::task::AtomicWaker` with `futures::executor::block_on`, though alternatives like `Mutex<Thread>` with thread parking would work.

To notify an observation that a collection is blocked, `_count` includes a “waiting flag” bit. After spinning unsuccessfully, collection registers a waker in `AtomicWaker`, sets the waiting flag while reading `_count` atomically, and attempts a new consistent read. If successful, the waker is unregistered, and the result is returned; otherwise, it waits for the waker to be called and repeats. On the observation side, incrementing `_count` with the waiting flag set triggers the registered waker.

### Cache Locality

Unlike the Go implementation, which increments a shared observation counter across shards, this algorithm performs all atomic increments within a single shard. Grouping a shard’s counters in the same cache line improves cache locality, beneficial for atomic [read-modify-write](https://en.wikipedia.org/wiki/Read%E2%80%93modify%E2%80%93write) operations that require [exclusive cache line access](https://en.wikipedia.org/wiki/MESI_protocol), as exclusivity acquisition is relatively costly.

While not trivial in safe Rust, as `Vec` alignment cannot be modified, this is achieved using a two-dimensional array with the inner array aligned.

### Testing

The algorithm’s correctness is validated under the [C++11 memory model](https://en.cppreference.com/w/cpp/atomic/memory_order) using both [`miri`](https://github.com/rust-lang/miri) and [`loom`](https://github.com/tokio-rs/loom).

### NaN Support

The current implementation panics on `NaN` inputs to `Histogram::observe`, but it can be trivially supported by adding a `NaN` bucket after the `+Inf` bucket, ignored in collection results.

## Disclaimer

This implementation was motivated by a project requiring millions of observations per second, where I needed to assess histogram latency impact. Reviewing the Rust Prometheus client source revealed a TODO linking to the Go client’s pull request. So I started to implement the Go algorithm into the Rust client, but I had the feeling doing it that there should be a way to use the redundancy between the bucket counters and the total counter. I have some experience in high-performance atomic algorithms, and the hot/cold shard algorithm is quite similar to the one I use in [`swap-buffer-queue`](https://github.com/wyfo/swap-buffer-queue). I was influenced by the sharding idea, but I managed to turn it differently by splitting the counters across shards instead of replicating them. Cache locality then came as an added benefit.

[^1]: There is no atomic fetch-and-add operation defined for floats, so it’s implemented using a compare-and-swap loop with an integer atomic field to store the float bytes.
[^2]: Although the Go scheduler differs from the Linux scheduler and goroutines are often more numerous than threads, Linus Torvalds’ arguments still apply. The `runtime.Gosched` function doesn’t seem to be designed for spin-lock mechanisms but rather for yielding in long-running, low-priority goroutines. A mechanism like this implementation’s, using a flag on the `_count` counter and a channel for notification, avoids these issues.