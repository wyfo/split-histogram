#![cfg_attr(not(any(feature = "unsafe", feature = "asm")), forbid(unsafe_code))]

#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{
    array,
    fmt::Error,
    future::poll_fn,
    iter,
    marker::PhantomData,
    mem,
    sync::{Arc, Mutex},
    task::Poll,
};

#[cfg(not(loom))]
use futures_executor::block_on;
#[cfg(not(loom))]
use futures_util::task::AtomicWaker;
#[cfg(loom)]
use loom::{
    future::{block_on, AtomicWaker},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
#[cfg(feature = "prometheus-client")]
use prometheus_client::{
    encoding::{EncodeMetric, MetricEncoder, NoLabelSet},
    metrics::{MetricType, TypedMetric},
};

mod impls;
#[cfg(test)]
mod tests;

pub trait HistogramValue {
    const HAS_NAN: bool;
    fn into_f64(self) -> f64;
    fn is_nan(&self) -> bool;
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering);
    fn from_bits(bits: u64) -> Self;
}

pub trait HistogramBuckets {
    type Value: HistogramValue;
    fn bucket_index(&self, value: &Self::Value) -> Option<usize>;
    fn values(&self) -> impl Iterator<Item = Self::Value>;
}
#[cfg(feature = "unsafe")]
/// # Safety
///
/// [`HistogramBuckets::bucket_index`] must return an index lesser than
/// the count of items returned by [`HistogramBuckets::values`]
pub unsafe trait TrustedHistogramBuckets: HistogramBuckets {}

#[derive(Debug)]
pub struct Histogram<B: HistogramBuckets = Vec<f64>, const TRUSTED_BUCKETS: bool = false>(
    Arc<HistogramInner<B>>,
);

impl<B: HistogramBuckets> Histogram<B> {
    pub fn new(buckets: B) -> Self {
        let bucket_count =
            buckets.values().count() + /* inf */ 1 + /* nan */ B::Value::HAS_NAN as usize;
        Self(Arc::new(HistogramInner {
            buckets,
            bucket_count,
            hot_shard: AtomicUsize::new(0),
            shards: array::from_fn(|_| Shard::new(bucket_count)),
            collector: Mutex::new(()),
            waker: AtomicWaker::new(),
        }))
    }
}

#[cfg(feature = "unsafe")]
impl<B: TrustedHistogramBuckets> Histogram<B, true> {
    pub fn new_trusted(buckets: B) -> Self {
        Self(Histogram::new(buckets).0)
    }
}

impl<B: HistogramBuckets, const TRUSTED_BUCKETS: bool> Histogram<B, TRUSTED_BUCKETS> {
    pub fn observe(&self, value: B::Value) {
        let buckets = &self.0.buckets;
        let fallback_bucket =
            || self.0.bucket_count - 1 - (B::Value::HAS_NAN && !value.is_nan()) as usize;
        let bucket_index = buckets.bucket_index(&value).unwrap_or_else(fallback_bucket);
        #[cfg(feature = "unsafe")]
        if !TRUSTED_BUCKETS {
            assert!(bucket_index < self.0.bucket_count);
        }
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed);
        #[cfg(feature = "unsafe")]
        if hot_shard > 1 {
            unsafe { std::hint::unreachable_unchecked() }
        }
        self.0.shards[hot_shard].observe(value, bucket_index, &self.0.waker);
    }

    pub fn collect(&self) -> (u64, f64, impl Iterator<Item = (f64, u64)>) {
        let _guard = self.0.collector.lock().unwrap();
        let hot_shard = self.0.hot_shard.load(Ordering::Relaxed);
        let cold_shard = hot_shard ^ 1;
        let (count_cold, sum_cold, buckets_cold) =
            self.0.shards[cold_shard].collect(self.0.bucket_count, &self.0.waker);
        self.0.hot_shard.store(cold_shard, Ordering::Relaxed);
        let (count_hot, sum_hot, buckets_hot) =
            self.0.shards[hot_shard].collect(self.0.bucket_count, &self.0.waker);
        let buckets = (self.0.buckets.values().map(B::Value::into_f64))
            .chain([f64::INFINITY])
            .zip(iter::zip(buckets_cold, buckets_hot))
            .map(|(b, (cold, hot))| (b, cold + hot));
        (count_cold + count_hot, sum_cold + sum_hot, buckets)
    }
}

impl<B: HistogramBuckets, const TRUSTED_BUCKET: bool> Clone for Histogram<B, TRUSTED_BUCKET> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Debug)]
struct HistogramInner<B: HistogramBuckets> {
    buckets: B,
    bucket_count: usize,
    hot_shard: AtomicUsize,
    shards: [Shard<B>; 2],
    collector: Mutex<()>,
    waker: AtomicWaker,
}

#[cfg(feature = "unsafe")]
impl<B: HistogramBuckets> Drop for HistogramInner<B> {
    fn drop(&mut self) {
        for shard in &mut self.shards {
            shard.drop(self.bucket_count);
        }
    }
}

trait HistogramCounters {
    fn new(bucket_count: usize) -> Self;
    fn count(&self) -> &AtomicU64;
    fn sum(&self) -> &AtomicU64;
    fn bucket(&self, bucket_index: usize) -> &AtomicU64;
    fn buckets(&self, bucket_count: usize) -> impl Iterator<Item = &AtomicU64>;
    #[cfg(feature = "unsafe")]
    fn drop(&mut self, bucket_count: usize) {
        let _ = bucket_count;
    }
}

#[derive(Debug)]
struct Shard<B> {
    counters: impls::Counters,
    _phantom: PhantomData<B>,
}

impl<B: HistogramBuckets> Shard<B> {
    const SPIN_LOOP_LIMIT: usize = if cfg!(not(loom)) { 10 } else { 1 };
    const WAITING_FLAG: u64 = 1 << (u64::BITS - 1);

    fn new(bucket_count: usize) -> Self {
        Self {
            counters: HistogramCounters::new(bucket_count),
            _phantom: PhantomData,
        }
    }

    fn observe(&self, value: B::Value, bucket_index: usize, waker: &AtomicWaker) {
        self.counters
            .bucket(bucket_index)
            .fetch_add(1, Ordering::Relaxed);
        B::Value::atomic_add(self.counters.sum(), value, Ordering::Release);
        let count = self.counters.count().fetch_add(1, Ordering::Release);
        if count & Self::WAITING_FLAG != 0 {
            #[cold]
            fn wake(waker: &AtomicWaker) {
                waker.wake();
            }
            wake(waker);
        }
    }

    fn read_sum_and_buckets(&self, buckets: &mut [u64]) -> (f64, u64) {
        let bucket_count = buckets.len();
        let sum = B::Value::from_bits(self.counters.sum().load(Ordering::Acquire)).into_f64();
        let mut expected_count = 0;
        for (count, counter) in buckets.iter_mut().zip(self.counters.buckets(bucket_count)) {
            *count = counter.load(Ordering::Relaxed);
            expected_count += *count;
        }
        (sum, expected_count)
    }

    fn collect(&self, bucket_count: usize, waker: &AtomicWaker) -> (u64, f64, Vec<u64>) {
        let mut buckets = vec![0; bucket_count];
        for _ in 0..Self::SPIN_LOOP_LIMIT {
            let count = self.counters.count().load(Ordering::Acquire) & !Self::WAITING_FLAG;
            let (sum, expected_count) = self.read_sum_and_buckets(&mut buckets);
            if count == expected_count {
                return (count, sum, buckets);
            }
        }
        self.collect_cold(&mut buckets, waker)
    }

    #[cold]
    fn collect_cold(&self, buckets: &mut Vec<u64>, waker: &AtomicWaker) -> (u64, f64, Vec<u64>) {
        block_on(poll_fn(move |cx| {
            #[cfg(not(loom))]
            waker.register(cx.waker());
            #[cfg(loom)]
            waker.register(cx.waker().clone());
            let count = (self.counters.count()).fetch_or(Self::WAITING_FLAG, Ordering::Acquire)
                & !Self::WAITING_FLAG;
            let (sum, expected_count) = self.read_sum_and_buckets(buckets);
            if count == expected_count {
                if (self.counters.count()).fetch_and(!Self::WAITING_FLAG, Ordering::Relaxed)
                    & Self::WAITING_FLAG
                    != 0
                {
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

    #[cfg(feature = "unsafe")]
    fn drop(&mut self, bucket_count: usize) {
        self.counters.drop(bucket_count);
    }
}

#[cfg(feature = "prometheus-client")]
impl<B: HistogramBuckets, const TRUSTED_BUCKETS: bool> TypedMetric
    for Histogram<B, TRUSTED_BUCKETS>
{
    const TYPE: MetricType = MetricType::Histogram;
}

#[cfg(feature = "prometheus-client")]
impl<B: HistogramBuckets, const TRUSTED_BUCKETS: bool> EncodeMetric
    for Histogram<B, TRUSTED_BUCKETS>
{
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), Error> {
        let (count, sum, buckets) = self.collect();
        encoder.encode_histogram::<NoLabelSet>(sum, count, &buckets.collect::<Vec<_>>(), None)
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }
}

#[cfg(feature = "asm")]
#[unsafe(no_mangle)]
pub fn observe_f64(h: &Histogram<&[f64], { cfg!(feature = "unsafe") }>, v: f64) {
    h.observe(v)
}

#[cfg(feature = "asm")]
#[unsafe(no_mangle)]
pub fn observe_u64(h: &Histogram<&[u64], { cfg!(feature = "unsafe") }>, v: u64) {
    h.observe(v)
}
