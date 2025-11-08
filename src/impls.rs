use super::{Arc, AtomicU64, Ordering};
use crate::{HistogramBuckets, HistogramValue};

impl HistogramValue for u64 {
    const HAS_NAN: bool = false;
    fn into_f64(self) -> f64 {
        self as f64
    }
    fn is_nan(&self) -> bool {
        false
    }
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter.fetch_add(value, ordering);
    }
    fn from_bits(bits: u64) -> Self {
        bits
    }
}

impl HistogramValue for f64 {
    const HAS_NAN: bool = true;
    fn into_f64(self) -> f64 {
        self
    }
    fn is_nan(&self) -> bool {
        f64::is_nan(*self)
    }
    fn atomic_add(counter: &AtomicU64, value: Self, ordering: Ordering) {
        counter
            .fetch_update(ordering, Ordering::Relaxed, |c| {
                Some(f64::to_bits(f64::from_bits(c) + value))
            })
            .unwrap();
    }
    fn from_bits(bits: u64) -> Self {
        f64::from_bits(bits)
    }
}

macro_rules! impl_buckets {
    ($($(@$N:ident)? $ty:ty),* $(,)?) => {$(
        impl<V: HistogramValue + PartialOrd + Clone + 'static, $(const $N: usize)?> HistogramBuckets for $ty {
            type Value = V;
            fn bucket_index(&self, value: &Self::Value) -> Option<usize> {
                self.iter().position(|b| value <= b)
            }
            fn values(&self) -> impl Iterator<Item = Self::Value> {
                self.iter().cloned()
            }
        }
        #[cfg(feature = "unsafe")]
        // SAFETY: `len` is constant and `bucket_index` is always in bounds
        unsafe impl<V: HistogramValue + PartialOrd + Clone + 'static, $(const $N: usize)?> crate::TrustedHistogramBuckets for $ty {}
    )*};
}
impl_buckets!(&[V], Vec<V>, Box<[V]>, Arc<[V]>, @N [V; N], @N &[V; N]);

#[cfg(not(any(feature = "unsafe", feature = "naive")))]
mod aligned {
    use std::iter;

    use crossbeam_utils::CachePadded;

    use super::AtomicU64;
    use crate::HistogramCounters;

    const COUNTERS_PER_CACHE_LINE: usize = align_of::<CachePadded<()>>() / align_of::<AtomicU64>();

    // all counters should be stored on the same cache line to optimize grouped atomic operations
    // they are stored in the following order:
    // - _count
    // - _sum
    // - _buckets
    #[derive(Debug)]
    pub(crate) struct Counters(Vec<CachePadded<[AtomicU64; COUNTERS_PER_CACHE_LINE]>>);

    impl HistogramCounters for Counters {
        fn new(bucket_count: usize) -> Self {
            let cache_lines =
                (/* _count + _sum */2 + bucket_count).div_ceil(COUNTERS_PER_CACHE_LINE);
            let vec = iter::repeat_with(Default::default)
                .take(cache_lines)
                .collect();
            Self(vec)
        }
        fn count(&self) -> &AtomicU64 {
            &self.0[0][0]
        }
        fn sum(&self) -> &AtomicU64 {
            &self.0[0][1]
        }
        fn bucket(&self, bucket_index: usize) -> &AtomicU64 {
            let idx = bucket_index + 2;
            &self.0[idx / COUNTERS_PER_CACHE_LINE][idx % COUNTERS_PER_CACHE_LINE]
        }
        fn buckets(&self, bucket_count: usize) -> impl Iterator<Item = &AtomicU64> {
            self.0
                .iter()
                .flat_map(|cache_line| cache_line.iter())
                .skip(2)
                .take(bucket_count)
        }
    }
}

#[cfg(all(feature = "naive", not(feature = "unsafe")))]
mod naive {
    use std::iter;

    use super::AtomicU64;
    use crate::HistogramCounters;

    #[derive(Debug)]
    pub(crate) struct Counters {
        count: AtomicU64,
        sum: AtomicU64,
        buckets: Vec<AtomicU64>,
    }

    #[cfg(feature = "naive")]
    impl HistogramCounters for Counters {
        fn new(bucket_count: usize) -> Self {
            Self {
                count: Default::default(),
                sum: Default::default(),
                buckets: iter::repeat_with(Default::default)
                    .take(bucket_count)
                    .collect(),
            }
        }
        fn count(&self) -> &AtomicU64 {
            &self.count
        }
        fn sum(&self) -> &AtomicU64 {
            &self.sum
        }
        fn bucket(&self, bucket_index: usize) -> &AtomicU64 {
            &self.buckets[bucket_index]
        }
        fn buckets(&self, bucket_count: usize) -> impl Iterator<Item = &AtomicU64> {
            self.buckets[..bucket_count].iter()
        }
    }
}

#[cfg(feature = "unsafe")]
mod r#unsafe {
    use std::{
        alloc,
        alloc::{alloc_zeroed, handle_alloc_error, Layout, LayoutError},
        slice,
    };

    use crossbeam_utils::CachePadded;

    use super::AtomicU64;
    use crate::HistogramCounters;

    #[derive(Debug)]
    pub(crate) struct Counters(*const UnsafeCountersInner);

    // SAFETY: raw pointer access is properly synchronized
    unsafe impl Send for Counters {}

    // SAFETY: raw pointer access is properly synchronized
    unsafe impl Sync for Counters {}

    #[repr(C)]
    struct UnsafeCountersInner {
        _align: CachePadded<()>,
        count: AtomicU64,
        sum: AtomicU64,
        buckets: [AtomicU64; 0],
    }

    impl Counters {
        fn layout(bucket_count: usize) -> Result<Layout, LayoutError> {
            let buckets_layout = Layout::array::<AtomicU64>(bucket_count)?;
            let (layout, _) = Layout::new::<UnsafeCountersInner>().extend(buckets_layout)?;
            Ok(layout)
        }

        fn buckets_ptr(&self) -> *const AtomicU64 {
            // Pointer has been properly initialized in `Self::new`
            unsafe { &raw const (*self.0).buckets }.cast()
        }
    }

    impl HistogramCounters for Counters {
        fn new(bucket_count: usize) -> Self {
            let Ok(layout) = Self::layout(bucket_count) else {
                panic!("capacity overflow");
            };
            // SAFETY: layout has non-zero size
            let inner = unsafe { alloc_zeroed(layout) };
            if inner.is_null() {
                handle_alloc_error(layout);
            }
            #[cfg(loom)]
            for i in 0..bucket_count + 2 {
                unsafe { inner.cast::<AtomicU64>().add(i).write(AtomicU64::new(0)) };
            }
            Self(inner.cast_const().cast())
        }

        fn count(&self) -> &AtomicU64 {
            // SAFETY: UnsafeCountersInner has been allocated and properly zero-initialized
            unsafe { &(*self.0).count }
        }

        fn sum(&self) -> &AtomicU64 {
            // SAFETY: UnsafeCountersInner has been allocated and properly zero-initialized
            unsafe { &(*self.0).sum }
        }

        fn bucket(&self, bucket_index: usize) -> &AtomicU64 {
            // SAFETY: UnsafeCountersInner has been allocated with an extended capacity of
            // `bucket_count`, is properly zero-initialized, and `bucket_index < bucket_count`
            unsafe { &*self.buckets_ptr().add(bucket_index) }
        }

        fn buckets(&self, bucket_count: usize) -> impl Iterator<Item = &AtomicU64> {
            // SAFETY: UnsafeCountersInner has been allocated with an extended capacity of
            // `bucket_count` and is properly zero-initialized
            unsafe { slice::from_raw_parts(self.buckets_ptr(), bucket_count) }.iter()
        }

        fn drop(&mut self, bucket_count: usize) {
            let layout = Self::layout(bucket_count).unwrap();
            // SAFETY: `self.0` was allocated with the same layout derived from `bucket_count`
            unsafe { alloc::dealloc(self.0.cast_mut().cast(), layout) }
        }
    }
}

#[cfg(not(any(feature = "unsafe", feature = "naive")))]
pub(crate) use aligned::Counters;
#[cfg(all(feature = "naive", not(feature = "unsafe")))]
pub(crate) use naive::Counters;
#[cfg(feature = "unsafe")]
pub(crate) use r#unsafe::Counters;
