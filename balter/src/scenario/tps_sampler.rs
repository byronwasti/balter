use crate::controllers::{CCOutcome, ConcurrencyController};
use crate::transaction::{TransactionData, TRANSACTION_HOOK};
use arc_swap::ArcSwap;
use balter_core::{SampleSet, TpsData};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use std::future::Future;
use std::{
    num::NonZeroU32,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
use tokio::time::{interval, Interval};
#[allow(unused)]
use tracing::{debug, error, info, trace};

const SAMPLE_WINDOW_SIZE: usize = 10;
const SKIP_SIZE: usize = 3;

pub(crate) struct ConcurrentSampler<T> {
    tps_sampler: TpsSampler<T>,
    cc: ConcurrencyController,
    samples: SampleSet,
    needs_clear: bool,
    tps_limited: bool,
}

impl<T, F> ConcurrentSampler<T>
where
    T: Fn() -> F + Send + Sync + 'static + Clone,
    F: Future<Output = ()> + Send,
{
    pub(crate) fn new(scenario: T, goal_tps: NonZeroU32) -> Self {
        Self {
            tps_sampler: TpsSampler::new(scenario, goal_tps),
            cc: ConcurrencyController::new(goal_tps),
            samples: SampleSet::new(SAMPLE_WINDOW_SIZE).skip_first_n(SKIP_SIZE),
            needs_clear: false,
            tps_limited: false,
        }
    }

    pub(crate) async fn get_samples(&mut self) -> Option<&SampleSet> {
        // NOTE: We delay clearing our samples to allow for the various controllers to potentially
        // lower the goal_tps. For instance, if we haven't reached our GoalTPS but the error rate
        // is too high, we don't want to adjust the concurrency and clear the data collected -- we
        // want to reset the goal.
        if self.needs_clear {
            info!("Clearing samples");
            self.samples.clear();
            self.needs_clear = false;
        }

        self.samples.push(self.tps_sampler.sample_tps().await);

        if self.samples.full() {
            match self.cc.analyze(&self.samples) {
                CCOutcome::Stable => {}
                CCOutcome::TpsLimited(max_tps, concurrency) => {
                    self.tps_limited = true;
                    self.set_concurrency(concurrency);
                    self.set_goal_tps_unchecked(max_tps);
                }
                CCOutcome::AlterConcurrency(concurrency) => {
                    self.set_concurrency(concurrency);
                }
            }

            Some(&self.samples)
        } else {
            None
        }
    }

    pub fn set_goal_tps(&mut self, goal_tps: NonZeroU32) {
        if self.tps_limited && goal_tps > self.tps_sampler.tps_limit {
            debug!("Unable to set TPS; TPS is limited");
        } else {
            self.set_goal_tps_unchecked(goal_tps);
        }
    }

    pub async fn wait_for_shutdown(self) {
        self.tps_sampler.wait_for_shutdown().await;
    }

    fn set_concurrency(&mut self, concurrency: usize) {
        self.needs_clear = true;
        info!("Setting concurrency to: {concurrency}");
        self.tps_sampler.set_concurrency(concurrency);
    }

    fn set_goal_tps_unchecked(&mut self, goal_tps: NonZeroU32) {
        if goal_tps != self.tps_sampler.tps_limit {
            self.needs_clear = true;
            self.cc.set_goal_tps(goal_tps);
            self.tps_sampler.set_tps_limit(goal_tps);
        }
    }
}

pub(crate) struct TpsSampler<T> {
    scenario: T,
    concurrency: Arc<AtomicUsize>,
    limiter: Arc<ArcSwap<DefaultDirectRateLimiter>>,
    tps_limit: NonZeroU32,

    tasks: Vec<JoinHandle<()>>,
    interval: Interval,
    last_tick: Instant,

    success_count: Arc<AtomicU64>,
    error_count: Arc<AtomicU64>,
}

impl<T, F> TpsSampler<T>
where
    T: Fn() -> F + Send + Sync + 'static + Clone,
    F: Future<Output = ()> + Send,
{
    pub(crate) fn new(scenario: T, tps_limit: NonZeroU32) -> Self {
        let limiter: DefaultDirectRateLimiter = rate_limiter(tps_limit);
        let limiter: Arc<DefaultDirectRateLimiter> = Arc::new(limiter);
        let limiter: Arc<ArcSwap<DefaultDirectRateLimiter>> = Arc::new(ArcSwap::new(limiter));
        let mut new_self = Self {
            scenario,
            concurrency: Arc::new(AtomicUsize::new(1)),
            limiter,
            tps_limit,

            tasks: vec![],
            interval: interval(Duration::from_millis(200)),
            last_tick: Instant::now(),

            success_count: Arc::new(AtomicU64::new(0)),
            error_count: Arc::new(AtomicU64::new(0)),
        };
        new_self.populate_jobs();
        new_self
    }

    pub(crate) async fn sample_tps(&mut self) -> TpsData {
        self.interval.tick().await;
        let success_count = self.success_count.swap(0, Ordering::Relaxed);
        let error_count = self.error_count.swap(0, Ordering::Relaxed);
        let elapsed = self.last_tick.elapsed();
        self.last_tick = Instant::now();

        let data = TpsData {
            elapsed,
            success_count,
            error_count,
        };

        // TODO: We should adjust interval timing based on noise not just sample count.
        if data.total() > 2_000 {
            let new_interval = self.interval.period() / 2;
            self.interval = interval(new_interval);
            // NOTE: First tick() is always instant
            self.interval.tick().await;
        }

        data
    }

    /// NOTE: Panics when concurrent_count=0
    pub(crate) fn set_concurrency(&mut self, concurrency: usize) {
        if concurrency != 0 {
            self.concurrency.store(concurrency, Ordering::Relaxed);
            self.populate_jobs();
        } else {
            panic!("Concurrent count is not allowed to be set to 0.");
        }
    }

    pub(crate) fn set_tps_limit(&mut self, tps_limit: NonZeroU32) {
        if tps_limit != self.tps_limit {
            self.tps_limit = tps_limit;
            self.limiter.store(Arc::new(rate_limiter(tps_limit)));
        }
    }

    pub(crate) async fn wait_for_shutdown(mut self) {
        self.concurrency.store(0, Ordering::Relaxed);
        for task in self.tasks.drain(..) {
            // TODO: Timeout in case a scenario loops indefinitely
            task.await.expect("Task unexpectedly failed.");
        }
    }

    fn populate_jobs(&mut self) {
        let concurrent_count = self.concurrency.load(Ordering::Relaxed);

        if self.tasks.len() > concurrent_count {
            // TODO: Clean up the tasks cleanly + timeout/abort in case a scenario loops
            // indefinitely
            self.tasks.truncate(concurrent_count);
        } else {
            while self.tasks.len() < concurrent_count {
                let scenario = self.scenario.clone();
                let concurrent_count = self.concurrency.clone();
                let id = self.tasks.len();
                let transaction_data = TransactionData {
                    // TODO: Use ArcSwap here
                    limiter: self.limiter.clone(),
                    success: self.success_count.clone(),
                    error: self.error_count.clone(),
                };

                trace!("Spawning a new task with id {id}.");
                self.tasks.push(tokio::spawn(TRANSACTION_HOOK.scope(
                    transaction_data,
                    async move {
                        while id < concurrent_count.load(Ordering::Relaxed) {
                            scenario().await;
                        }
                    },
                )));
            }
        }
    }
}

fn rate_limiter(tps_limit: NonZeroU32) -> DefaultDirectRateLimiter {
    RateLimiter::direct(
        Quota::per_second(tps_limit)
            // TODO: Make burst configurable
            .allow_burst(NonZeroU32::new(1).unwrap()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_distr::{Distribution, Normal};

    async fn mock_trivial_scenario() {
        let _ = crate::transaction::transaction_hook::<_, (), ()>(async { Ok(()) }).await;
    }

    async fn mock_noisy_scenario() {
        let _ = crate::transaction::transaction_hook::<_, (), ()>(async {
            let normal = Normal::new(100., 25.).unwrap();
            let v: f64 = normal.sample(&mut rand::thread_rng());
            tokio::time::sleep(Duration::from_micros(v.floor() as u64)).await;
            Ok(())
        })
        .await;
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    #[ignore]
    #[ntest::timeout(300)]
    async fn test_simple_case() {
        let mut tps_sampler =
            TpsSampler::new(mock_trivial_scenario, NonZeroU32::new(1_000).unwrap());
        tps_sampler.set_concurrency(20);

        let _sample = tps_sampler.sample_tps().await;
        for _ in 0..10 {
            let sample = tps_sampler.sample_tps().await;
            info!("tps: {}", sample.tps());
            assert!((1_000. - sample.tps()).abs() < 150.);
        }
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    #[ignore]
    #[ntest::timeout(300)]
    async fn test_noisy_case() {
        let mut tps_sampler = TpsSampler::new(mock_noisy_scenario, NonZeroU32::new(1_000).unwrap());
        tps_sampler.set_concurrency(20);

        let _sample = tps_sampler.sample_tps().await;
        for _ in 0..10 {
            let sample = tps_sampler.sample_tps().await;
            info!("tps: {}", sample.tps());
            assert!((1_000. - sample.tps()).abs() < 150.);
        }
    }
}