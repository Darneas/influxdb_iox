use data_types::job::Job;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    ops::DerefMut,
    sync::Arc,
    time::Duration,
};
use time::TimeProvider;
use tracker::{TaskId, TaskRegistration, TaskRegistryWithHistory, TaskTracker, TrackedFutureExt};

const JOB_HISTORY_SIZE: usize = 1000;

/// The global job registry
#[derive(Debug)]
pub struct JobRegistry {
    inner: Mutex<JobRegistryInner>,
}

#[derive(Debug)]
pub struct JobRegistryInner {
    registry: TaskRegistryWithHistory<Job>,
    metrics: JobRegistryMetrics,
}

// impl JobRegistry {
//     pub fn new(
//         metric_registry: Arc<metric::Registry>,
//         time_provider: Arc<dyn TimeProvider>,
//     ) -> Self {
//         Self {
//             inner: Mutex::new(JobRegistryInner {
//                 registry: TaskRegistryWithHistory::new(time_provider, JOB_HISTORY_SIZE),
//                 metrics: JobRegistryMetrics::new(metric_registry),
//             }),
//         }
//     }

//     pub fn register(&self, job: Job) -> (TaskTracker<Job>, TaskRegistration) {
//         self.inner.lock().registry.register(job)
//     }

//     /// Returns a list of recent Jobs, including a history of past jobs
//     pub fn tracked(&self) -> Vec<TaskTracker<Job>> {
//         self.inner.lock().registry.tracked()
//     }

//     /// Returns the list of running Jobs
//     pub fn running(&self) -> Vec<TaskTracker<Job>> {
//         self.inner.lock().registry.running()
//     }

//     pub fn get(&self, id: TaskId) -> Option<TaskTracker<Job>> {
//         self.inner.lock().registry.get(id)
//     }

//     pub fn spawn_dummy_job(&self, nanos: Vec<u64>, db_name: Option<Arc<str>>) -> TaskTracker<Job> {
//         let (tracker, registration) = self.register(Job::Dummy {
//             nanos: nanos.clone(),
//             db_name,
//         });

//         for duration in nanos {
//             tokio::spawn(
//                 async move {
//                     tokio::time::sleep(tokio::time::Duration::from_nanos(duration)).await;
//                     Ok::<_, Infallible>(())
//                 }
//                 .track(registration.clone()),
//             );
//         }

//         tracker
//     }

//     /// Reclaims jobs into the historical archive
//     ///
//     /// Returns the number of remaining jobs
//     ///
//     /// Should be called periodically
//     pub(crate) fn reclaim(&self) -> usize {
//         let mut lock = self.inner.lock();
//         let mut_ref = lock.deref_mut();

//         let (_reclaimed, pruned) = mut_ref.registry.reclaim();
//         mut_ref.metrics.update(&mut_ref.registry, pruned);
//         mut_ref.registry.tracked_len()
//     }
// }

#[derive(Debug)]
struct JobRegistryMetrics {
    active_gauge: metric::Metric<metric::U64Gauge>,

    // Accumulates jobs that were pruned from the limited job history. This is required to not saturate the completed
    // count after a while.
    completed_accu: BTreeMap<metric::Attributes, u64>,

    cpu_time_histogram: metric::Metric<metric::DurationHistogram>,
    wall_time_histogram: metric::Metric<metric::DurationHistogram>,

    // Set of jobs for which we already accounted data but that are still tracked. We must not account these
    // jobs a second time.
    completed_but_still_tracked: BTreeSet<TaskId>,
}

// impl JobRegistryMetrics {
//     fn new(metric_registry: Arc<metric::Registry>) -> Self {
//         Self {
//             active_gauge: metric_registry
//                 .register_metric("influxdb_iox_job_count", "Number of known jobs"),
//             completed_accu: Default::default(),
//             cpu_time_histogram: metric_registry.register_metric_with_options(
//                 "influxdb_iox_job_completed_cpu",
//                 "CPU time of of completed jobs",
//                 Self::duration_histogram_options,
//             ),
//             wall_time_histogram: metric_registry.register_metric_with_options(
//                 "influxdb_iox_job_completed_wall",
//                 "Wall time of of completed jobs",
//                 Self::duration_histogram_options,
//             ),
//             completed_but_still_tracked: Default::default(),
//         }
//     }

//     fn duration_histogram_options() -> metric::DurationHistogramOptions {
//         metric::DurationHistogramOptions::new(vec![
//             Duration::from_millis(10),
//             Duration::from_millis(100),
//             Duration::from_secs(1),
//             Duration::from_secs(10),
//             Duration::from_secs(100),
//             metric::DURATION_MAX,
//         ])
//     }

//     fn update(&mut self, registry: &TaskRegistryWithHistory<Job>, pruned: Vec<TaskTracker<Job>>) {
//         // scan pruned jobs
//         for job in pruned {
//             assert!(job.is_complete());
//             if self.completed_but_still_tracked.remove(&job.id()) {
//                 // already accounted
//                 continue;
//             }

//             self.process_completed_job(&job);
//         }

//         // scan current completed jobs
//         let (tracked_completed, tracked_other): (Vec<_>, Vec<_>) = registry
//             .tracked()
//             .into_iter()
//             .partition(|job| job.is_complete());
//         for job in tracked_completed {
//             if !self.completed_but_still_tracked.insert(job.id()) {
//                 // already accounted
//                 continue;
//             }

//             self.process_completed_job(&job);
//         }

//         // scan current not-completed jobs
//         let mut accumulator: BTreeMap<metric::Attributes, u64> = self.completed_accu.clone();
//         for job in tracked_other {
//             let attr = Self::job_to_gauge_attributes(&job);
//             accumulator
//                 .entry(attr.clone())
//                 .and_modify(|x| *x += 1)
//                 .or_insert(1);
//         }

//         // emit metric
//         for (attr, count) in accumulator {
//             self.active_gauge.recorder(attr).set(count);
//         }
//     }

//     fn job_to_gauge_attributes(job: &TaskTracker<Job>) -> metric::Attributes {
//         let metadata = job.metadata();
//         let status = job.get_status();

//         let mut attributes = metric::Attributes::from(&[
//             ("description", metadata.description()),
//             (
//                 "status",
//                 status
//                     .result()
//                     .map(|result| result.name())
//                     .unwrap_or_else(|| status.name()),
//             ),
//         ]);
//         if let Some(db_name) = metadata.db_name() {
//             attributes.insert("db_name", db_name.to_string());
//         }

//         attributes
//     }

//     fn process_completed_job(&mut self, job: &TaskTracker<Job>) {
//         let attr = Self::job_to_gauge_attributes(job);
//         self.completed_accu
//             .entry(attr.clone())
//             .and_modify(|x| *x += 1)
//             .or_insert(1);

//         let status = job.get_status();
//         if let Some(nanos) = status.cpu_nanos() {
//             self.cpu_time_histogram
//                 .recorder(attr.clone())
//                 .record(std::time::Duration::from_nanos(nanos as u64));
//         }
//         if let Some(nanos) = status.wall_nanos() {
//             self.wall_time_histogram
//                 .recorder(attr)
//                 .record(std::time::Duration::from_nanos(nanos as u64));
//         }
//     }
// }
