use super::ScenarioConfig;
use crate::controllers::concurrency::{ConcurrencyController, Message};
#[cfg(feature = "rt")]
use crate::runtime::BALTER_OUT;
use crate::tps_sampler::TpsSampler;
use std::future::Future;
use std::num::NonZeroU32;
#[allow(unused_imports)]
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn, Instrument};
use metrics::gauge;

#[instrument(name="scenario", skip_all, fields(name=config.name))]
pub(crate) async fn run_tps<T, F>(scenario: T, config: ScenarioConfig)
where
    T: Fn() -> F + Send + Sync + 'static + Clone,
    F: Future<Output = ()> + Send,
{
    info!("Running {} with config {:?}", config.name, &config);

    let start = Instant::now();

    let goal_tps = config.goal_tps().unwrap();
    let mut controller = ConcurrencyController::new(NonZeroU32::new(goal_tps).unwrap());
    let mut sampler = TpsSampler::new(scenario, NonZeroU32::new(goal_tps).unwrap());
    sampler.set_concurrent_count(controller.concurrency());

    let metrics_label = format!("{}-concurrency", config.name);
    let goal_label = format!("{}-goal_tps", config.name);
    gauge!(metrics_label.clone()).set(controller.concurrency() as f64);
    gauge!(goal_label.clone()).set(goal_tps as f64);

    // NOTE: This loop is time-sensitive. Any long awaits or blocking will throw off measurements
    loop {
        let sample = sampler.sample_tps().await;
        if start.elapsed() > config.duration {
            break;
        }

        match controller.analyze(sample.tps()) {
            Message::None | Message::Stable => {}
            Message::AlterConcurrency(val) => {
                gauge!(metrics_label.clone()).set(val as f64);
                sampler.set_concurrent_count(val);
            }
            Message::TpsLimited(max_tps) => {
                sampler.set_tps_limit(max_tps);
                gauge!(metrics_label.clone()).set(controller.concurrency() as f64);
                gauge!(goal_label.clone()).set(max_tps.get() as f64);

                #[cfg(feature = "rt")]
                distribute_work(&config, start.elapsed(), u32::from(max_tps) as f64).await;
            }
        }
    }
    sampler.wait_for_shutdown().await;
    gauge!(metrics_label.clone()).set(0.);
    gauge!(goal_label.clone()).set(0.);

    info!("Scenario complete");
}

#[cfg(feature = "rt")]
async fn distribute_work(config: &ScenarioConfig, elapsed: Duration, self_tps: f64) {
    let mut new_config = config.clone();
    // TODO: This does not take into account transmission time. Logic will have
    // to be far fancier to properly time-sync various peers on a single
    // scenario.
    new_config.duration = config.duration - elapsed;

    let new_tps = new_config.goal_tps().unwrap() - self_tps as u32;
    new_config.set_goal_tps(new_tps);

    tokio::spawn(async move {
        let (ref tx, _) = *BALTER_OUT;
        // TODO: Handle the error case.
        let _ = tx.send(new_config).await;
    });
}
