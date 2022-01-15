//! Tooling to remove parts of the preserved catalog that are no longer needed.
use std::{collections::BTreeMap, sync::Arc};

use futures::TryStreamExt;
use iox_object_store::{IoxObjectStore, TransactionFilePath};
use object_store::{ObjectStore, ObjectStoreApi};
use snafu::{ResultExt, Snafu};
use time::Time;

use crate::{
    internals::{proto_io::load_transaction_proto, proto_parse::parse_timestamp},
    ProtoIOError, ProtoParseError,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Error during store read operation: {}", source))]
    Read {
        source: <ObjectStore as ObjectStoreApi>::Error,
    },

    #[snafu(display("Error during store delete operation: {}", source))]
    Delete {
        source: <ObjectStore as ObjectStoreApi>::Error,
    },

    #[snafu(display("Error during protobuf IO: {}", source))]
    ProtobufIOError { source: ProtoIOError },

    #[snafu(display("Internal: Error while parsing protobuf: {}", source))]
    ProtobufParseError { source: ProtoParseError },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Prune history of [`PreservedCatalog`](crate::core::PreservedCatalog).
///
/// This deletes all transactions and checkpoints that were started prior to `before`. Note that this only deletes data
/// that is safe to delete when time travel to `before` is allowed. For example image the following transactions:
///
/// | Timestamp | What                      |
/// | --------- | ------------------------- |
/// | C1        | Checkpoint                |
/// | T2        | Transaction               |
/// | T3, C3    | Transaction + Checkpoint  |
/// | T4        | Transaction               |
/// | `before`  |                           |
/// | T5        | Transaction               |
/// | T6, C6    | Transaction + Checkpoint  |
/// | T7        | Transaction               |
/// | T9, C9    | Transaction + Checkpoint  |
///
/// This will delete the following content: C1, T2, and T3. C3 and T4 cannot be deleted because it is required to
/// recover T5 which is AFTER `before`.
pub async fn prune_history(iox_object_store: Arc<IoxObjectStore>, before: Time) -> Result<()> {
    // collect all files so we can quickly filter them later for deletion
    // Use a btree-map so we can iterate from oldest to newest revision.
    let mut files: BTreeMap<u64, Vec<TransactionFilePath>> = Default::default();

    // remember latest checkpoint is <= before
    let mut latest_in_prune_range: Option<u64> = None;

    let mut stream = iox_object_store
        .catalog_transaction_files()
        .await
        .context(ReadSnafu)?;

    while let Some(transaction_file_list) = stream.try_next().await.context(ReadSnafu)? {
        for transaction_file_path in transaction_file_list {
            if is_checkpoint_or_zero(&transaction_file_path) {
                let proto = load_transaction_proto(&iox_object_store, &transaction_file_path)
                    .await
                    .context(ProtobufIOSnafu)?;
                let start_timestamp =
                    parse_timestamp(&proto.start_timestamp).context(ProtobufParseSnafu)?;

                if start_timestamp <= before {
                    latest_in_prune_range = Some(
                        latest_in_prune_range.map_or(transaction_file_path.revision_counter, |x| {
                            x.max(transaction_file_path.revision_counter)
                        }),
                    );
                }
            }

            files
                .entry(transaction_file_path.revision_counter)
                .and_modify(|known| known.push(transaction_file_path))
                .or_insert_with(|| vec![transaction_file_path]);
        }
    }

    if let Some(earliest_keep) = latest_in_prune_range {
        for (revision, files) in files {
            if revision > earliest_keep {
                break;
            }

            for file in files {
                if (file.revision_counter < earliest_keep) || !is_checkpoint_or_zero(&file) {
                    iox_object_store
                        .delete_catalog_transaction_file(&file)
                        .await
                        .context(DeleteSnafu)?;
                }
            }
        }
    }

    Ok(())
}

/// Check if given path is represents a checkpoint or is revision zero.
///
/// For both cases this file can be used to start to read a catalog.
fn is_checkpoint_or_zero(path: &TransactionFilePath) -> bool {
    path.is_checkpoint() || (path.revision_counter == 0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use parquet_file::test_utils::make_iox_object_store;

    use crate::{
        core::PreservedCatalog,
        interface::CheckpointData,
        test_helpers::{load_ok, make_config, new_empty},
    };

    use super::*;

    #[tokio::test]
    async fn test_empty_store() {
        let iox_object_store = make_iox_object_store().await;

        prune_history(iox_object_store, Time::from_timestamp_nanos(0))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_do_delete_wipe_last_checkpoint() {
        let config = make_config().await;

        new_empty(config.clone()).await;

        prune_history(
            Arc::clone(&config.iox_object_store),
            Time::from_timestamp_nanos(0),
        )
        .await
        .unwrap();

        load_ok(config).await.unwrap();
    }

    #[tokio::test]
    async fn test_complex_1() {
        let time = Arc::new(time::MockProvider::new(Time::from_timestamp(0, 32)));
        let config = make_config()
            .await
            .with_time_provider(Arc::<time::MockProvider>::clone(&time));

        let iox_object_store = &config.iox_object_store;

        let catalog = new_empty(config.clone()).await;
        create_transaction(&catalog).await;
        create_transaction_and_checkpoint(&catalog).await;

        let before = time.inc(Duration::from_secs(21));
        time.inc(Duration::from_secs(1));

        create_transaction(&catalog).await;

        prune_history(Arc::clone(iox_object_store), before)
            .await
            .unwrap();

        assert_eq!(
            known_revisions(iox_object_store).await,
            vec![(2, true), (3, false)],
        );
    }

    #[tokio::test]
    async fn test_complex_2() {
        let time = Arc::new(time::MockProvider::new(Time::from_timestamp(0, 32)));
        let config = make_config()
            .await
            .with_time_provider(Arc::<time::MockProvider>::clone(&time));

        let iox_object_store = &config.iox_object_store;

        let catalog = new_empty(config.clone()).await;

        create_transaction(&catalog).await;
        create_transaction_and_checkpoint(&catalog).await;
        create_transaction(&catalog).await;

        let before = time.inc(Duration::from_secs(25));
        time.inc(Duration::from_secs(1));

        create_transaction(&catalog).await;
        create_transaction_and_checkpoint(&catalog).await;
        create_transaction(&catalog).await;

        prune_history(Arc::clone(iox_object_store), before)
            .await
            .unwrap();

        assert_eq!(
            known_revisions(iox_object_store).await,
            vec![
                (2, true),
                (3, false),
                (4, false),
                (5, false),
                (5, true),
                (6, false)
            ],
        );
    }

    #[tokio::test]
    async fn test_keep_all() {
        let config = make_config().await;
        let iox_object_store = &config.iox_object_store;

        let catalog = new_empty(config.clone()).await;
        create_transaction(&catalog).await;
        create_transaction_and_checkpoint(&catalog).await;
        create_transaction(&catalog).await;

        let before = config.time_provider.now() - Duration::from_secs(1_000);
        prune_history(Arc::clone(iox_object_store), before)
            .await
            .unwrap();

        assert_eq!(
            known_revisions(iox_object_store).await,
            vec![(0, false), (1, false), (2, false), (2, true), (3, false)],
        );
    }

    async fn create_transaction(catalog: &PreservedCatalog) {
        let t = catalog.open_transaction().await;
        t.commit().await.unwrap();
    }

    async fn create_transaction_and_checkpoint(catalog: &PreservedCatalog) {
        let t = catalog.open_transaction().await;
        let ckpt_handle = t.commit().await.unwrap();
        ckpt_handle
            .create_checkpoint(CheckpointData {
                files: Default::default(),
                delete_predicates: Default::default(),
            })
            .await
            .unwrap();
    }

    async fn known_revisions(iox_object_store: &IoxObjectStore) -> Vec<(u64, bool)> {
        let mut revisions = iox_object_store
            .catalog_transaction_files()
            .await
            .unwrap()
            .map_ok(|files| {
                files
                    .into_iter()
                    .map(|f| (f.revision_counter, f.is_checkpoint()))
                    .collect::<Vec<(u64, bool)>>()
            })
            .try_concat()
            .await
            .unwrap();

        revisions.sort_unstable();

        revisions
    }
}
