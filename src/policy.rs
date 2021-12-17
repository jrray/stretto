#[cfg(test)]
mod test;

use crate::{
    bbloom::Bloom,
    error::CacheError,
    metrics::{MetricType, Metrics},
    sketch::CountMinSketch,
};
use parking_lot::Mutex;
use std::{
    collections::{hash_map::RandomState, BinaryHeap, HashMap},
    hash::BuildHasher,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

/// DEFAULT_SAMPLES is the number of items to sample when looking at eviction
/// candidates. 5 seems to be the most optimal number [citation needed].
const DEFAULT_SAMPLES: usize = 5;

macro_rules! impl_policy {
    ($policy: ident) => {
        use crate::policy::DEFAULT_SAMPLES;
        use crate::policy::{PolicyPair, PolicyPairOrd};
        use std::collections::BinaryHeap;

        impl $policy {
            #[inline]
            pub(crate) fn new(ctrs: usize, max_cost: i64) -> Result<Self, CacheError> {
                Self::with_hasher(ctrs, max_cost, RandomState::new())
            }
        }

        impl<S: BuildHasher + Clone + 'static> $policy<S> {
            #[inline]
            pub fn collect_metrics(&mut self, metrics: Arc<Metrics>) {
                self.metrics = metrics.clone();
                self.inner.lock().set_metrics(metrics);
            }

            pub fn add(&self, key: u64, cost: i64) -> (Option<Vec<PolicyPair>>, bool) {
                let mut inner = self.inner.lock();
                let max_cost = inner.costs.get_max_cost();

                // cannot ad an item bigger than entire cache
                if cost > max_cost {
                    return (None, false);
                }

                // no need to go any further if the item is already in the cache
                if inner.costs.update(&key, cost) {
                    // an update does not count as an addition, so return false.
                    return (None, false);
                }

                // If the execution reaches this point, the key doesn't exist in the cache.
                // Calculate the remaining room in the cache (usually bytes).
                let mut room = inner.costs.room_left(cost);
                if room >= 0 {
                    // There's enough room in the cache to store the new item without
                    // overflowing. Do that now and stop here.
                    inner.costs.increment(key, cost);
                    self.metrics.add(MetricType::CostAdd, key, cost as u64);
                    return (None, true);
                }

                // inc_hits is the hit count for the incoming item
                let inc_hits = inner.admit.estimate(key);
                // sample is the eviction candidate pool to be filled via random sampling.
                let mut sample = BinaryHeap::with_capacity(DEFAULT_SAMPLES);
                let mut victims = Vec::new();

                // Delete victims until there's enough space or a minKey is found that has
                // more hits than incoming item.
                while room < 0 {
                    // fill up empty slots in sample
                    sample = inner.fill_sample(sample);

                    // find minimally used key
                    // note that this removes from the heap as well
                    match sample.pop() {
                        Some(policy_pair_ord) => {
                            let PolicyPairOrd {
                                policy_pair,
                                hits: min_hits,
                            } = policy_pair_ord;
                            let PolicyPair {
                                key: min_key,
                                cost: min_cost,
                            } = policy_pair;
                            // If the incoming item isn't worth keeping in the policy, reject.
                            if inc_hits < min_hits {
                                self.metrics.add(MetricType::RejectSets, key, 1);
                                return (Some(victims), false);
                            }

                            // Delete the victim from metadata.
                            inner.costs.remove(&min_key).map(|cost| {
                                self.metrics
                                    .add(MetricType::CostEvict, min_key, cost as u64);
                                self.metrics.add(MetricType::KeyEvict, min_key, 1);
                            });

                            // store victim in evicted victims slice
                            victims.push(PolicyPair::new(min_key, min_cost));

                            room = inner.costs.room_left(cost);
                        }
                        _ => {}
                    }
                }

                inner.costs.increment(key, cost);
                self.metrics.add(MetricType::CostAdd, key, cost as u64);
                (Some(victims), true)
            }

            #[inline]
            pub fn contains(&self, k: &u64) -> bool {
                let inner = self.inner.lock();
                inner.costs.contains(k)
            }

            #[inline]
            pub fn remove(&self, k: &u64) {
                let mut inner = self.inner.lock();
                inner.costs.remove(k).map(|cost| {
                    self.metrics.add(MetricType::CostEvict, *k, cost as u64);
                    self.metrics.add(MetricType::KeyEvict, *k, 1);
                });
            }

            #[inline]
            pub fn cap(&self) -> i64 {
                let inner = self.inner.lock();
                inner.costs.get_max_cost() - inner.costs.used
            }

            #[inline]
            pub fn update(&self, k: &u64, cost: i64) {
                let mut inner = self.inner.lock();
                inner.costs.update(k, cost);
            }

            #[inline]
            pub fn cost(&self, k: &u64) -> i64 {
                let inner = self.inner.lock();
                inner.costs.key_costs.get(k).map_or(-1, |cost| *cost)
            }

            #[inline]
            pub fn clear(&self) {
                let mut inner = self.inner.lock();
                inner.admit.clear();
                inner.costs.clear();
            }

            #[inline]
            pub fn max_cost(&self) -> i64 {
                let inner = self.inner.lock();
                inner.costs.get_max_cost()
            }

            #[inline]
            pub fn update_max_cost(&self, mc: i64) {
                let inner = self.inner.lock();
                inner.costs.update_max_cost(mc)
            }
        }

        unsafe impl<S: BuildHasher + Clone + 'static> Send for $policy<S> {}
        unsafe impl<S: BuildHasher + Clone + 'static> Sync for $policy<S> {}
    };
}

cfg_sync!(
    mod sync;
    pub(crate) use sync::LFUPolicy;
);

cfg_async!(
    mod axync;
    pub(crate) use axync::AsyncLFUPolicy;
);

pub(crate) struct PolicyInner<S = RandomState> {
    admit: TinyLFU,
    costs: SampledLFU<S>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PolicyPair {
    pub(crate) key: u64,
    pub(crate) cost: i64,
}

impl PolicyPair {
    #[inline]
    fn new(k: u64, c: i64) -> Self {
        Self { key: k, cost: c }
    }
}

impl From<(u64, i64)> for PolicyPair {
    fn from(pair: (u64, i64)) -> Self {
        Self {
            key: pair.0,
            cost: pair.1,
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd)]
pub(crate) struct PolicyPairOrd {
    pub(crate) policy_pair: PolicyPair,
    pub(crate) hits: i64,
}

impl PolicyPairOrd {
    fn new(policy_pair: PolicyPair, hits: i64) -> PolicyPairOrd {
        PolicyPairOrd { policy_pair, hits }
    }
}

impl Ord for PolicyPairOrd {
    fn cmp(&self, other: &PolicyPairOrd) -> std::cmp::Ordering {
        self.hits.cmp(&other.hits)
    }
}

impl<S: BuildHasher + Clone + 'static> PolicyInner<S> {
    #[inline]
    fn set_metrics(&mut self, metrics: Arc<Metrics>) {
        self.costs.metrics = metrics;
    }

    #[inline]
    fn with_hasher(ctrs: usize, max_cost: i64, hasher: S) -> Result<Arc<Mutex<Self>>, CacheError> {
        let this = Self {
            admit: TinyLFU::new(ctrs)?,
            costs: SampledLFU::with_hasher(max_cost, hasher),
        };
        Ok(Arc::new(Mutex::new(this)))
    }

    fn fill_sample(&self, mut pairs: BinaryHeap<PolicyPairOrd>) -> BinaryHeap<PolicyPairOrd> {
        if pairs.len() >= self.costs.samples {
            pairs
        } else {
            for (k, v) in self.costs.key_costs.iter() {
                let hits = self.admit.estimate(*k);
                pairs.push(PolicyPairOrd::new(PolicyPair::new(*k, *v), hits));
            }
            pairs
        }
    }
}

unsafe impl<S: BuildHasher + Clone + 'static> Send for PolicyInner<S> {}
unsafe impl<S: BuildHasher + Clone + 'static> Sync for PolicyInner<S> {}

/// SampledLFU stores key-costs paris.
pub(crate) struct SampledLFU<S = RandomState> {
    samples: usize,
    max_cost: AtomicI64,
    used: i64,
    key_costs: HashMap<u64, i64, S>,
    metrics: Arc<Metrics>,
}

impl SampledLFU {
    /// Create a new SampledLFU
    pub fn new(max_cost: i64) -> Self {
        Self {
            samples: DEFAULT_SAMPLES,
            max_cost: AtomicI64::new(max_cost),
            used: 0,
            key_costs: HashMap::new(),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// Create a new SampledLFU with samples.
    #[inline]
    pub fn with_samples(max_cost: i64, samples: usize) -> Self {
        Self {
            samples,
            max_cost: AtomicI64::new(max_cost),
            used: 0,
            key_costs: HashMap::new(),
            metrics: Arc::new(Metrics::new()),
        }
    }
}

impl<S: BuildHasher + Clone + 'static> SampledLFU<S> {
    /// Create a new SampledLFU with specific hasher
    #[inline]
    pub fn with_hasher(max_cost: i64, hasher: S) -> Self {
        Self {
            samples: DEFAULT_SAMPLES,
            max_cost: AtomicI64::new(max_cost),
            used: 0,
            key_costs: HashMap::with_hasher(hasher),
            metrics: Arc::new(Metrics::Noop),
        }
    }

    /// Create a new SampledLFU with samples and hasher
    #[inline]
    pub fn with_samples_and_hasher(max_cost: i64, samples: usize, hasher: S) -> Self {
        Self {
            samples,
            max_cost: AtomicI64::new(max_cost),
            used: 0,
            key_costs: HashMap::with_hasher(hasher),
            metrics: Arc::new(Metrics::Noop),
        }
    }

    /// Update the max_cost
    #[inline]
    pub fn update_max_cost(&self, mc: i64) {
        self.max_cost.store(mc, Ordering::SeqCst);
    }

    /// get the max_cost
    #[inline]
    pub fn get_max_cost(&self) -> i64 {
        self.max_cost.load(Ordering::SeqCst)
    }

    /// get the remain space of SampledLRU
    #[inline]
    pub fn room_left(&self, cost: i64) -> i64 {
        self.get_max_cost() - (self.used + cost)
    }

    /// try to fill the SampledLFU by the given pairs.
    pub fn fill_sample(&mut self, mut pairs: Vec<PolicyPair>) -> Vec<PolicyPair> {
        if pairs.len() >= self.samples {
            pairs
        } else {
            for (k, v) in self.key_costs.iter() {
                pairs.push(PolicyPair::new(*k, *v));
                if pairs.len() >= self.samples {
                    return pairs;
                }
            }
            pairs
        }
    }

    /// Put a hashed key and cost to SampledLFU
    #[inline]
    pub fn increment(&mut self, key: u64, cost: i64) {
        self.key_costs.insert(key, cost);
        self.used += cost;
    }

    /// Remove an entry from SampledLFU by hashed key
    #[inline]
    pub fn remove(&mut self, kh: &u64) -> Option<i64> {
        self.key_costs.remove(kh).map(|cost| {
            self.used -= cost;
            cost
        })
    }

    #[inline]
    pub fn contains(&self, k: &u64) -> bool {
        self.key_costs.contains_key(k)
    }

    /// Clear the SampledLFU
    #[inline]
    pub fn clear(&mut self) {
        self.used = 0;
        self.key_costs.clear();
    }

    /// Update the cost by hashed key. If the provided key in SampledLFU, then update it and return true, otherwise false.
    #[inline]
    pub fn update(&mut self, k: &u64, cost: i64) -> bool {
        // Update the cost of an existing key, but don't worry about evicting.
        // Evictions will be handled the next time a new item is added
        match self.key_costs.get_mut(k) {
            None => false,
            Some(prev) => {
                let prev_val = *prev;
                let k = *k;
                if self.metrics.is_op() {
                    self.metrics.add(MetricType::KeyUpdate, k, 1);
                    match prev_val.cmp(&cost) {
                        std::cmp::Ordering::Less => {
                            let diff = (cost - prev_val) as u64;
                            self.metrics.add(MetricType::CostAdd, k, diff);
                        }
                        std::cmp::Ordering::Equal => {}
                        std::cmp::Ordering::Greater => {
                            let diff = (prev_val - cost) as u64 - 1;
                            self.metrics.add(MetricType::CostAdd, k, !diff);
                        }
                    }
                }

                self.used += cost - prev_val;
                *prev = cost;
                true
            }
        }
    }
}

unsafe impl<S: BuildHasher + Clone + 'static> Send for SampledLFU<S> {}
unsafe impl<S: BuildHasher + Clone + 'static> Sync for SampledLFU<S> {}

/// TinyLFU is an admission helper that keeps track of access frequency using
/// tiny (4-bit) counters in the form of a count-min sketch.
pub(crate) struct TinyLFU {
    ctr: CountMinSketch,
    doorkeeper: Bloom,
    samples: usize,
    w: usize,
}

impl TinyLFU {
    /// The constructor of TinyLFU
    #[inline]
    pub fn new(num_ctrs: usize) -> Result<Self, CacheError> {
        Ok(Self {
            ctr: CountMinSketch::new(num_ctrs as u64)?,
            doorkeeper: Bloom::new(num_ctrs, 0.01),
            samples: num_ctrs,
            w: 0,
        })
    }

    /// estimates the frequency.of key hash
    ///
    /// # Details
    /// Explanation from [TinyLFU: A Highly Efficient Cache Admission Policy §3.4.2]:
    /// - When querying items, we use both the Doorkeeper and the main structures.
    /// That is, if the item is included in the Doorkeeper,
    /// TinyLFU estimates the frequency of this item as its estimation in the main structure plus 1.
    /// Otherwise, TinyLFU returns just the estimation from the main structure.
    ///
    /// [TinyLFU: A Highly Efficient Cache Admission Policy §3.4.2]: https://arxiv.org/pdf/1512.00727.pdf
    #[inline]
    pub fn estimate(&self, kh: u64) -> i64 {
        let mut hits = self.ctr.estimate(kh);
        if self.doorkeeper.contains(kh) {
            hits += 1;
        }
        hits
    }

    /// increment multiple hashed keys, for details, please see [`increment_hash`].
    ///
    /// [`increment`]: struct.TinyLFU.method.increment.html
    #[inline]
    pub fn increments(&mut self, khs: Vec<u64>) {
        khs.iter().for_each(|k| self.increment(*k))
    }

    /// See [TinyLFU: A Highly Efficient Cache Admission Policy] §3.2
    ///
    /// [TinyLFU: A Highly Efficient Cache Admission Policy]: https://arxiv.org/pdf/1512.00727.pdf
    #[inline]
    pub fn increment(&mut self, kh: u64) {
        // Flip doorkeeper bit if not already done.
        if !self.doorkeeper.contains_or_add(kh) {
            // Increment count-min counter if doorkeeper bit is already set.
            self.ctr.increment(kh);
        }

        self.try_reset();
    }

    /// See [TinyLFU: A Highly Efficient Cache Admission Policy] §3.2 and §3.3
    ///
    /// [TinyLFU: A Highly Efficient Cache Admission Policy]: https://arxiv.org/pdf/1512.00727.pdf
    #[inline]
    pub fn try_reset(&mut self) {
        self.w += 1;
        if self.w >= self.samples {
            self.reset();
        }
    }

    #[inline]
    fn reset(&mut self) {
        // zero out size
        self.w = 0;

        // zero bloom filter bits
        self.doorkeeper.reset();

        // halves count-min counters
        self.ctr.reset();
    }

    /// `clear` is an extension for the original TinyLFU.
    ///
    /// Comparing to [`reset`] halves the all the bits of count-min sketch,
    /// `clear` will set all the bits to zero of count-min sketch
    ///
    /// [`reset`]: struct.TinyLFU.method.reset.html
    #[inline]
    pub fn clear(&mut self) {
        self.w = 0;
        self.doorkeeper.clear();
        self.ctr.clear();
    }

    /// `contains` checks if bit(s) for entry hash is/are set,
    /// returns true if the hash was added to the TinyLFU.
    #[inline]
    pub fn contains(&self, kh: u64) -> bool {
        self.doorkeeper.contains(kh)
    }
}
