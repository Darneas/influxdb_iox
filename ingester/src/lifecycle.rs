//! Manages the persistence and eviction lifecycle of data in the buffer across all sequencers.
//! Note that the byte counts logged by the lifecycle manager and when exactly persistence gets
//! triggered aren't required to be absolutely accurate. The byte count is just an estimate
//! anyway, this just needs to keep things moving along to keep memory use roughly under
//! some absolute number and individual Parquet files that get persisted below some number. It
//! is expected that they may be above or below the absolute thresholds.

use crate::data::Persister;
use iox_catalog::interface::PartitionId;
use observability_deps::tracing::error;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_PARTITION_SIZE_THRESHOLD: usize = 1_024 * 1_024 * 300; // 300MB
pub(crate) const DEFAULT_AGE_THRESHOLD: Duration = Duration::from_secs(60 * 30); // 30 minutes

/// The lifecycle manager keeps track of the size and age of partitions across all sequencers.
/// It triggers persistence based on keeping total memory usage around a set amount while
/// ensuring that partitions don't get too old or large before being persisted.
pub(crate) struct LifecycleManager {
    pause_ingest_size: usize,
    persist_memory_threshold: usize,
    partition_size_threshold: usize,
    partition_age_threshold: Duration,
    state: Mutex<LifecycleState>,
}

#[derive(Default, Debug)]
struct LifecycleState {
    total_bytes: usize,
    partition_stats: BTreeMap<PartitionId, PartitionLifecycleStats>,
}

impl LifecycleState {
    fn remove(&mut self, partition_id: &PartitionId) {
        if let Some(stats) = self.partition_stats.remove(partition_id) {
            self.total_bytes -= stats.bytes_written;
        }
    }
}

/// A snapshot of the stats for the lifecycle manager
#[derive(Debug)]
pub struct LifecycleStats {
    /// total number of bytes the lifecycle manager is aware of across all sequencers and
    /// partitions. Based on the mutable batch sizes received into all partitions.
    pub total_bytes: usize,
    /// the stats for every partition the lifecycle manager is tracking.
    pub partition_stats: Vec<PartitionLifecycleStats>,
}

/// The stats for a partition
#[derive(Debug, Clone, Copy)]
pub struct PartitionLifecycleStats {
    /// The partition identifier
    partition_id: PartitionId,
    /// Instant that the partition received its first write. This is reset anytime
    /// the partition is persisted.
    first_write: Instant,
    /// The number of bytes in the partition as estimated by the mutable batch sizes.
    bytes_written: usize,
}

impl LifecycleManager {
    /// Initialize a new lifecycle manager that will persist when `maybe_persist` is called
    /// if anything is over the size or age threshold.
    pub(crate) fn new(
        pause_ingest_size: usize,
        persist_memory_threshold: usize,
        partition_size_threshold: usize,
        partition_age_threshold: Duration,
    ) -> Self {
        // this must be true to ensure that persistence will get triggered, freeing up memory
        assert!(pause_ingest_size > persist_memory_threshold);

        Self {
            pause_ingest_size,
            persist_memory_threshold,
            partition_size_threshold,
            partition_age_threshold,
            state: Default::default(),
        }
    }

    /// Logs bytes written into a partition so that it can be tracked for the manager to
    /// trigger persistence. Returns true if the ingester should pause consuming from the
    /// write buffer so that persistence can catch up and free up memory.
    pub fn log_write(&self, partition_id: PartitionId, bytes_written: usize) -> bool {
        let mut s = self.state.lock();
        s.partition_stats
            .entry(partition_id)
            .or_insert_with(|| PartitionLifecycleStats {
                partition_id,
                first_write: Instant::now(),
                bytes_written: 0,
            })
            .bytes_written += bytes_written;
        s.total_bytes += bytes_written;

        s.total_bytes > self.pause_ingest_size
    }

    /// Returns true if the `total_bytes` tracked by the manager is less than the pause amount.
    /// As persistence runs, the `total_bytes` go down.
    pub fn can_resume_ingest(&self) -> bool {
        let s = self.state.lock();
        s.total_bytes < self.pause_ingest_size
    }

    /// This will persist any partitions that are over their size or age thresholds and
    /// persist as many partitions as necessary (largest first) to get below the memory threshold.
    /// The persist operations are spawned in new tasks and run at the same time, but the
    /// function waits for all to return before completing.
    pub async fn maybe_persist<P: Persister>(&self, persister: &Arc<P>) {
        let LifecycleStats {
            mut total_bytes,
            partition_stats,
        } = self.stats();

        // get anything over the threshold size or age to persist
        let now = Instant::now();
        let (to_persist, mut rest): (Vec<PartitionLifecycleStats>, Vec<PartitionLifecycleStats>) =
            partition_stats.into_iter().partition(|s| {
                now.duration_since(s.first_write) > self.partition_age_threshold
                    || s.bytes_written > self.partition_size_threshold
            });

        let mut persist_tasks: Vec<_> = to_persist
            .into_iter()
            .map(|s| {
                self.remove(s.partition_id);
                total_bytes -= s.bytes_written;
                let persister = Arc::clone(persister);

                tokio::task::spawn(async move {
                    if let Err(e) = persister.persist(s.partition_id).await {
                        error!(
                            %e,
                            "Error while persisting partition",
                        )
                    }
                })
            })
            .collect();

        // if we're still over the memory threshold, persist as many of the largest partitions
        // until we're under.
        if total_bytes > self.persist_memory_threshold {
            let mut to_persist = vec![];

            rest.sort_by(|a, b| b.bytes_written.cmp(&a.bytes_written));

            for s in rest {
                total_bytes -= s.bytes_written;
                to_persist.push(s);
                if total_bytes < self.persist_memory_threshold {
                    break;
                }
            }

            let mut to_persist: Vec<_> = to_persist
                .into_iter()
                .map(|s| {
                    self.remove(s.partition_id);
                    let persister = Arc::clone(persister);
                    tokio::task::spawn(async move {
                        if let Err(e) = persister.persist(s.partition_id).await {
                            error!(
                                %e,
                                "Error while persisting partition",
                            )
                        }
                    })
                })
                .collect();

            persist_tasks.append(&mut to_persist);
        }

        let persists = futures::future::join_all(persist_tasks.into_iter());
        persists.await;
    }

    /// Returns a point in time snapshot of the lifecycle state.
    pub fn stats(&self) -> LifecycleStats {
        let s = self.state.lock();
        let partition_stats: Vec<_> = s.partition_stats.values().cloned().collect();

        LifecycleStats {
            total_bytes: s.total_bytes,
            partition_stats,
        }
    }

    /// Removes the partition from the state
    pub fn remove(&self, partition_id: PartitionId) {
        let mut s = self.state.lock();
        s.remove(&partition_id);
    }
}

const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Runs the lifecycle manager to trigger persistence every second.
pub(crate) async fn run_lifecycle_manager<P: Persister>(
    manager: Arc<LifecycleManager>,
    persister: Arc<P>,
) {
    loop {
        manager.maybe_persist(&persister).await;
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::BTreeSet;

    #[derive(Default)]
    struct TestPersister {
        persist_called: Mutex<BTreeSet<PartitionId>>,
    }

    #[async_trait]
    impl Persister for TestPersister {
        async fn persist(&self, partition_id: PartitionId) -> crate::data::Result<()> {
            let mut p = self.persist_called.lock();
            p.insert(partition_id);
            Ok(())
        }
    }

    impl TestPersister {
        fn persist_called_for(&self, partition_id: PartitionId) -> bool {
            let p = self.persist_called.lock();
            p.contains(&partition_id)
        }
    }

    #[test]
    fn logs_write() {
        let m = LifecycleManager::new(20, 10, 5, Duration::from_nanos(0));
        let start = Instant::now();

        assert!(!m.log_write(PartitionId::new(1), 1));
        assert!(!m.log_write(PartitionId::new(1), 1));

        let mid = Instant::now();
        assert!(!m.log_write(PartitionId::new(2), 3));

        let stats = m.stats();
        assert_eq!(stats.total_bytes, 5);

        let p1 = stats.partition_stats.get(0).unwrap();
        assert_eq!(p1.bytes_written, 2);
        assert_eq!(p1.partition_id, PartitionId::new(1));
        assert!(p1.first_write > start);
        assert!(p1.first_write < Instant::now());

        let p2 = stats.partition_stats.get(1).unwrap();
        assert_eq!(p2.bytes_written, 3);
        assert_eq!(p2.partition_id, PartitionId::new(2));
        assert!(p2.first_write > mid);
        assert!(p2.first_write < Instant::now());
    }

    #[test]
    fn pausing_and_resuming_ingest() {
        let m = LifecycleManager::new(20, 10, 5, Duration::from_nanos(0));

        assert!(!m.log_write(PartitionId::new(1), 15));

        // now it should indicate a pause
        assert!(m.log_write(PartitionId::new(1), 10));
        assert!(!m.can_resume_ingest());

        m.remove(PartitionId::new(1));
        assert!(m.can_resume_ingest());
        assert!(!m.log_write(PartitionId::new(1), 3));
    }

    #[tokio::test]
    async fn persists_based_on_age() {
        let m = LifecycleManager::new(30, 20, 10, Duration::from_millis(5));
        let partition_id = PartitionId::new(1);
        let persister = Arc::new(TestPersister::default());
        m.log_write(partition_id, 10);

        m.maybe_persist(&persister).await;
        let stats = m.stats();
        assert_eq!(stats.total_bytes, 10);
        assert_eq!(stats.partition_stats[0].partition_id, partition_id);

        // age out the partition
        tokio::time::sleep(Duration::from_millis(6)).await;

        // validate that from before, persist wasn't called for the partition
        assert!(!persister.persist_called_for(partition_id));

        // write in data for a new partition so we can be sure it isn't persisted, but the older one is
        m.log_write(PartitionId::new(2), 6);

        m.maybe_persist(&persister).await;
        // give it a moment so that persist actually gets called
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(persister.persist_called_for(partition_id));
        assert!(!persister.persist_called_for(PartitionId::new(2)));

        let stats = m.stats();
        assert_eq!(stats.total_bytes, 6);
        assert_eq!(stats.partition_stats.len(), 1);
        assert_eq!(stats.partition_stats[0].partition_id, PartitionId::new(2));
    }

    #[tokio::test]
    async fn persists_based_on_partition_size() {
        let m = LifecycleManager::new(30, 20, 5, Duration::from_millis(1000));
        let partition_id = PartitionId::new(1);
        let persister = Arc::new(TestPersister::default());
        m.log_write(partition_id, 4);

        m.maybe_persist(&persister).await;
        // give it a moment so that persist actually gets called, if it were going to (it shouldn't)
        tokio::time::sleep(Duration::from_millis(10)).await;

        let stats = m.stats();
        assert_eq!(stats.total_bytes, 4);
        assert_eq!(stats.partition_stats[0].partition_id, partition_id);
        assert!(!persister.persist_called_for(partition_id));

        // introduce a new partition under the limit to verify it doesn't get taken with the other
        m.log_write(PartitionId::new(2), 3);
        m.log_write(partition_id, 5);

        m.maybe_persist(&persister).await;
        // give it a moment so that persist actually gets called
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(persister.persist_called_for(partition_id));
        assert!(!persister.persist_called_for(PartitionId::new(2)));

        let stats = m.stats();
        assert_eq!(stats.total_bytes, 3);
        assert_eq!(stats.partition_stats.len(), 1);
        assert_eq!(stats.partition_stats[0].partition_id, PartitionId::new(2));
    }

    #[tokio::test]
    async fn persists_based_on_memory_size() {
        let m = LifecycleManager::new(60, 20, 20, Duration::from_millis(1000));
        let partition_id = PartitionId::new(1);
        let persister = Arc::new(TestPersister::default());
        m.log_write(partition_id, 8);
        m.log_write(PartitionId::new(2), 13);

        m.maybe_persist(&persister).await;
        // give it a moment to ensure persist is called
        tokio::time::sleep(Duration::from_millis(10)).await;

        // the bigger of the two partitions should have been persisted, leaving the smaller behind
        let stats = m.stats();
        assert_eq!(stats.total_bytes, 8);
        assert_eq!(stats.partition_stats[0].partition_id, partition_id);
        assert!(!persister.persist_called_for(partition_id));
        assert!(persister.persist_called_for(PartitionId::new(2)));

        // add that partition back in over size
        m.log_write(partition_id, 20);
        m.log_write(PartitionId::new(2), 21);

        // both partitions should now need to be persisted to bring us below the mem threshold of 20.
        m.maybe_persist(&persister).await;
        // give it a moment so that persist actually gets called
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(persister.persist_called_for(partition_id));
        assert!(persister.persist_called_for(PartitionId::new(2)));

        let stats = m.stats();
        assert_eq!(stats.total_bytes, 0);
        assert_eq!(stats.partition_stats.len(), 0);
    }

    #[tokio::test]
    async fn persist_based_on_partition_and_memory_size() {
        let m = LifecycleManager::new(60, 6, 5, Duration::from_millis(1000));
        let persister = Arc::new(TestPersister::default());
        m.log_write(PartitionId::new(1), 4);
        m.log_write(PartitionId::new(2), 6);
        m.log_write(PartitionId::new(3), 3);

        m.maybe_persist(&persister).await;
        // give it a moment to ensure persist is called
        tokio::time::sleep(Duration::from_millis(10)).await;

        // the bigger of the two partitions should have been persisted, leaving the smaller behind
        let stats = m.stats();
        assert_eq!(stats.total_bytes, 3);
        assert_eq!(stats.partition_stats[0].partition_id, PartitionId::new(3));
        assert!(!persister.persist_called_for(PartitionId::new(3)));
        assert!(persister.persist_called_for(PartitionId::new(2)));
        assert!(persister.persist_called_for(PartitionId::new(1)));
    }
}
