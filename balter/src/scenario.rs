//! Scenario logic and constants
use balter_core::{
    config::{ScenarioConfig, ScenarioKind},
    stats::RunStatistics,
};
use std::{
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

mod direct;
mod goal_tps;
mod saturate;

/// The default error rate used for `.saturate()`
pub const DEFAULT_SATURATE_ERROR_RATE: f64 = 0.03;

/// The default error rate used for `.overload()`
pub const DEFAULT_OVERLOAD_ERROR_RATE: f64 = 0.80;

/// Load test scenario structure
///
/// Handler for running scenarios. Not intended for manual creation, use the [`#[scenario]`](balter_macros::scenario) macro which will add these methods to functions.
#[pin_project::pin_project]
pub struct Scenario<T> {
    func: T,
    runner_fut: Option<Pin<Box<dyn Future<Output = RunStatistics> + Send>>>,
    config: ScenarioConfig,
}

impl<T> Scenario<T> {
    #[doc(hidden)]
    pub fn new(name: &str, func: T) -> Self {
        Self {
            func,
            runner_fut: None,
            config: ScenarioConfig::new(name),
        }
    }
}

impl<T, F> Future for Scenario<T>
where
    T: Fn() -> F + Send + 'static + Clone + Sync,
    F: Future<Output = ()> + Send,
{
    type Output = RunStatistics;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.runner_fut.is_none() {
            let func = self.func.clone();
            let config = self.config.clone();
            self.runner_fut = Some(Box::pin(async move { run_scenario(func, config).await }));
        }

        if let Some(runner) = &mut self.runner_fut {
            runner.as_mut().poll(cx)
        } else {
            unreachable!()
        }
    }
}

pub trait ConfigurableScenario<T: Send>: Future<Output = T> + Sized + Send {
    fn saturate(self) -> Self;
    fn overload(self) -> Self;
    fn error_rate(self, error_rate: f64) -> Self;
    fn tps(self, tps: u32) -> Self;
    fn direct(self, tps_limit: u32, concurrency: usize) -> Self;
    fn duration(self, duration: Duration) -> Self;
}

impl<T, F> ConfigurableScenario<RunStatistics> for Scenario<T>
where
    T: Fn() -> F + Send + 'static + Clone + Sync,
    F: Future<Output = ()> + Send,
{
    /// Run the scenario increasing TPS until an error rate of 3% is reached.
    ///
    /// NOTE: Must supply a `.duration()` as well
    ///
    /// # Example
    /// ```no_run
    /// use balter::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     my_scenario()
    ///         .saturate()
    ///         .duration(Duration::from_secs(120))
    ///         .await;
    /// }
    ///
    /// #[scenario]
    /// async fn my_scenario() {
    /// }
    /// ```
    fn saturate(mut self) -> Self {
        self.config.kind = ScenarioKind::Saturate(DEFAULT_SATURATE_ERROR_RATE);
        self
    }

    /// Run the scenario increasing TPS until an error rate of 80% is reached.
    ///
    /// NOTE: Must supply a `.duration()` as well
    ///
    /// # Example
    /// ```no_run
    /// use balter::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     my_scenario()
    ///         .overload()
    ///         .duration(Duration::from_secs(120))
    ///         .await;
    /// }
    ///
    /// #[scenario]
    /// async fn my_scenario() {
    /// }
    /// ```
    fn overload(mut self) -> Self {
        self.config.kind = ScenarioKind::Saturate(DEFAULT_OVERLOAD_ERROR_RATE);
        self
    }

    /// Run the scenario increasing TPS until a custom error rate is reached.
    ///
    /// NOTE: Must supply a `.duration()` as well
    ///
    /// # Example
    /// ```no_run
    /// use balter::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     my_scenario()
    ///         .error_rate(0.25) // 25% error rate
    ///         .duration(Duration::from_secs(120))
    ///         .await;
    /// }
    ///
    /// #[scenario]
    /// async fn my_scenario() {
    /// }
    /// ```
    fn error_rate(mut self, error_rate: f64) -> Self {
        self.config.kind = ScenarioKind::Saturate(error_rate);
        self
    }

    /// Run the scenario at the specified TPS.
    ///
    /// NOTE: Must supply a `.duration()` as well
    ///
    /// # Example
    /// ```no_run
    /// use balter::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     my_scenario()
    ///         .tps(632)
    ///         .duration(Duration::from_secs(120))
    ///         .await;
    /// }
    ///
    /// #[scenario]
    /// async fn my_scenario() {
    /// }
    /// ```
    fn tps(mut self, tps: u32) -> Self {
        self.config.kind = ScenarioKind::Tps(tps);
        self
    }

    /// Run the scenario with direct control over TPS and concurrency.
    /// No automatic controls will limit or change any values. This is intended
    /// for development testing or advanced ussage.
    fn direct(mut self, tps_limit: u32, concurrency: usize) -> Self {
        self.config.kind = ScenarioKind::Direct(tps_limit, concurrency);
        self
    }

    /// Run the scenario for the given duration.
    ///
    /// NOTE: Must include one of `.tps()`/`.saturate()`/`.overload()`/`.error_rate()`
    ///
    /// # Example
    /// ```no_run
    /// use balter::prelude::*;
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     my_scenario()
    ///         .tps(10)
    ///         .duration(Duration::from_secs(120))
    ///         .await;
    /// }
    ///
    /// #[scenario]
    /// async fn my_scenario() {
    /// }
    /// ```
    fn duration(mut self, duration: Duration) -> Self {
        self.config.duration = duration;
        self
    }
}

#[cfg(feature = "rt")]
mod runtime {
    use super::*;
    use balter_runtime::DistributedScenario;

    impl<T, F> DistributedScenario for Scenario<T>
    where
        T: Fn() -> F + Send + 'static + Clone + Sync,
        F: Future<Output = ()> + Send,
    {
        #[allow(unused)]
        fn set_config(
            &self,
            config: ScenarioConfig,
        ) -> Pin<Box<dyn DistributedScenario<Output = Self::Output>>> {
            Box::pin(Scenario {
                func: self.func.clone(),
                runner_fut: None,
                config,
            })
        }
    }
}

async fn run_scenario<T, F>(scenario: T, config: ScenarioConfig) -> RunStatistics
where
    T: Fn() -> F + Send + Sync + 'static + Clone,
    F: Future<Output = ()> + Send,
{
    match config.kind {
        ScenarioKind::Once => {
            scenario().await;
            // TODO: Gather these for a single run
            RunStatistics {
                concurrency: 1,
                goal_tps: NonZeroU32::new(1).unwrap(),
                stable: true,
            }
        }
        ScenarioKind::Tps(_) => goal_tps::run_tps(scenario, config).await,
        ScenarioKind::Saturate(_) => saturate::run_saturate(scenario, config).await,
        ScenarioKind::Direct(_, _) => direct::run_direct(scenario, config).await,
    }
}
