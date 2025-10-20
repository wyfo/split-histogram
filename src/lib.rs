#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::{
    array,
    future::poll_fn,
    iter,
    marker::PhantomData,
    mem,
    ops::Add,
    sync::{Arc, Mutex},
    task::Poll,
};

use crossbeam_utils::CachePadded;
#[cfg(not(loom))]
use futures_executor::block_on;
#[cfg(not(loom))]
use futures_util::task::AtomicWaker;
#[cfg(loom)]
use loom::{
    future::{block_on, AtomicWaker},
    sync::atomic::{AtomicU64, AtomicU8, Ordering},
};

pub trait HistogramValue: Clone + Add<Output = Self> + PartialOrd + Sized {
    const MAX: Self;
    fn from_bits(bits: u64) -> Self;
    fn inc_by(counter: &AtomicU64, value: Self, ordering: Ordering);
}

impl HistogramValue for u64 {
    const MAX: Self = u64::MAX;
    fn from_bits(bits: u64) -> Self {
        bits
    }

    fn inc_by(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter.fetch_add(value, ordering);
    }
}

impl HistogramValue for f64 {
    const MAX: Self = f64::INFINITY;
    fn from_bits(bits: u64) -> Self {
        f64::from_bits(bits)
    }

    fn inc_by(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter
            .fetch_update(ordering, Ordering::Relaxed, |c| {
                Some(f64::to_bits(f64::from_bits(c) + value))
            })
            .unwrap();
    }
}

#[derive(Debug)]
pub struct Histogram<T = f64>(Arc<HistogramInner<T>>);

impl<T: HistogramValue> Histogram<T> {
    pub fn new(buckets: impl IntoIterator<Item = T>) -> Self {
        let buckets = buckets
            .into_iter()
            .chain(iter::once(T::MAX))
            .collect::<Vec<_>>();
        let shards = array::from_fn(|_| Shard::new(buckets.len()));
        Self(Arc::new(HistogramInner {
            buckets,
            shard_index: AtomicU8::new(0),
            shards,
            collector: Mutex::new(()),
            waker: AtomicWaker::new(),
        }))
    }

    pub fn observe(&self, value: T) {
        let bucket_index = self.0.buckets.iter().position(|b| value <= *b).unwrap();
        let shard_index = self.0.shard_index.load(Ordering::Relaxed) as usize;
        self.0.shards[shard_index].observe(value, bucket_index, &self.0.waker);
    }

    pub fn collect(&self) -> (u64, T, Vec<(T, u64)>) {
        let _guard = self.0.collector.lock().unwrap();
        self.0.shard_index.store(1, Ordering::Relaxed);
        let bucket_count = self.0.buckets.len();
        let (count0, sum0, buckets0) = self.0.shards[0].collect(bucket_count, &self.0.waker);
        self.0.shard_index.store(0, Ordering::Relaxed);
        let (count1, sum1, buckets1) = self.0.shards[1].collect(bucket_count, &self.0.waker);
        (
            count0 + count1,
            sum0 + sum1,
            (self.0.buckets.iter())
                .zip(iter::zip(buckets0, buckets1))
                .map(|(b, (c0, c1))| (b.clone(), c0 + c1))
                .collect(),
        )
    }
}

impl<T> Clone for Histogram<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Debug)]
pub struct HistogramInner<T> {
    buckets: Vec<T>,
    shard_index: AtomicU8,
    shards: [Shard<T>; 2],
    collector: Mutex<()>,
    waker: AtomicWaker,
}

const COUNTERS_PER_CACHE_LINE: usize = align_of::<CachePadded<()>>() / align_of::<AtomicU64>();
#[cfg(not(loom))]
const SPIN_LOOP_LIMIT: usize = 10;
#[cfg(loom)]
const SPIN_LOOP_LIMIT: usize = 1;
const PARKED_FLAG: u64 = 1 << (u64::BITS - 1);
const COUNT_MASK: u64 = !PARKED_FLAG;

#[derive(Debug)]
pub struct Shard<T> {
    // all counters should be stored on the same cache line to optimize grouped atomic operations
    // they are stored in the following order:
    // - _count
    // - _sum
    // - _buckets
    counters: Vec<CachePadded<[AtomicU64; COUNTERS_PER_CACHE_LINE]>>,
    _phantom: PhantomData<T>,
}

impl<T: HistogramValue> Shard<T> {
    fn new(bucket_count: usize) -> Self {
        Self {
            counters: (0..(bucket_count + 2).div_ceil(COUNTERS_PER_CACHE_LINE))
                .map(|_| CachePadded::new(array::from_fn(|_| AtomicU64::new(0))))
                .collect(),
            _phantom: PhantomData,
        }
    }

    fn count(&self) -> &AtomicU64 {
        &self.counters[0][0]
    }

    fn sum(&self) -> &AtomicU64 {
        &self.counters[0][1]
    }

    fn bucket(&self, bucket_index: usize) -> &AtomicU64 {
        let true_index = bucket_index + 2;
        &self.counters[true_index / COUNTERS_PER_CACHE_LINE][true_index % COUNTERS_PER_CACHE_LINE]
    }

    fn buckets(&self, bucket_count: usize) -> impl IntoIterator<Item = &AtomicU64> {
        self.counters
            .iter()
            .flat_map(|cache_line| cache_line.iter())
            .skip(2)
            .take(bucket_count)
    }

    fn observe(&self, value: T, bucket_index: usize, waker: &AtomicWaker) {
        self.bucket(bucket_index).fetch_add(1, Ordering::Relaxed);
        T::inc_by(self.sum(), value, Ordering::Release);
        let count = self.count().fetch_add(1, Ordering::AcqRel);
        if count & PARKED_FLAG != 0 {
            #[cold]
            fn unpark(waker: &AtomicWaker) {
                waker.wake();
            }
            unpark(waker);
        }
    }

    fn read_sum_and_buckets(&self, buckets: &mut [u64]) -> (T, u64) {
        let bucket_count = buckets.len();
        let sum = T::from_bits(self.sum().load(Ordering::Acquire));
        let mut expected_count = 0;
        for (count, counter) in buckets.iter_mut().zip(self.buckets(bucket_count)) {
            *count = counter.load(Ordering::Relaxed);
            expected_count += *count;
        }
        (sum, expected_count)
    }

    fn collect(&self, bucket_count: usize, waker: &AtomicWaker) -> (u64, T, Vec<u64>) {
        let mut buckets = vec![0; bucket_count];
        for _ in 0..SPIN_LOOP_LIMIT {
            let count = self.count().load(Ordering::Acquire) & COUNT_MASK;
            let (sum, expected_count) = self.read_sum_and_buckets(&mut buckets);
            if count == expected_count {
                return (count, sum, buckets);
            }
        }
        self.collect_cold(&mut buckets, waker)
    }

    #[cold]
    fn collect_cold(&self, buckets: &mut Vec<u64>, waker: &AtomicWaker) -> (u64, T, Vec<u64>) {
        block_on(poll_fn(move |cx| {
            #[cfg(not(loom))]
            waker.register(cx.waker());
            #[cfg(loom)]
            waker.register(cx.waker().clone());
            let count = self.count().fetch_or(PARKED_FLAG, Ordering::AcqRel) & COUNT_MASK;
            let (sum, expected_count) = self.read_sum_and_buckets(buckets);
            if count == expected_count {
                if self.count().fetch_and(COUNT_MASK, Ordering::Relaxed) & PARKED_FLAG != 0 {
                    #[cfg(not(loom))]
                    waker.take();
                    #[cfg(loom)]
                    waker.take_waker();
                }
                return Poll::Ready((count, sum, mem::take(buckets)));
            }
            Poll::Pending
        }))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(loom))]
    use std::thread;

    #[cfg(loom)]
    use loom::{model, thread};

    use crate::Histogram;

    #[cfg(not(loom))]
    fn model(f: impl Fn()) {
        f();
    }

    #[test]
    fn observe_and_collect() {
        model(|| {
            let histogram = Histogram::new([10, 100]);
            let h1 = histogram.clone();
            let h2 = histogram.clone();
            let t1 = thread::spawn(move || h1.observe(42));
            let t2 = thread::spawn(move || h2.collect());
            histogram.observe(7);
            histogram.observe(80100);
            t1.join().unwrap();
            let (count, sum, buckets) = t2.join().unwrap();
            assert!(count <= 3);
            let [(10, b0), (100, b1), (u64::MAX, b2)] = buckets[..] else {
                unreachable!()
            };
            assert_eq!(count, b0 + b1 + b2);
            let if_bucket = |b, v| if b == 1 { v } else { 0 };
            assert_eq!(
                sum,
                if_bucket(b0, 7) + if_bucket(b1, 42) + if_bucket(b2, 80100)
            );
            let (count, sum, buckets) = histogram.collect();
            assert_eq!(count, 3);
            assert_eq!(sum, 80149);
            assert_eq!(buckets, vec![(10, 1), (100, 1), (u64::MAX, 1)]);
        });
    }

    #[cfg(not(loom))]
    #[test]
    fn observe_inf() {
        let histogram = Histogram::new([0.1, 1.0]);
        histogram.observe(f64::INFINITY);
        histogram.observe(f64::NEG_INFINITY);
    }

    #[cfg(not(loom))]
    #[test]
    #[should_panic]
    fn observe_nan() {
        let histogram = Histogram::new([0.1, 1.0]);
        histogram.observe(f64::NAN);
    }
}
