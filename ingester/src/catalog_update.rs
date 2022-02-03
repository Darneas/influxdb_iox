//! Module to update the catalog during ingesting

use crate::{data::PersistingBatch, handler::IngestHandlerImpl};
use iox_catalog::interface::{ParquetFile, ProcessedTombstone, Tombstone};
use parquet_file::metadata::IoxMetadata;
use snafu::{ResultExt, Snafu};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub enum Error {
    #[snafu(display(
        "The Object Store ID of the Persisting Batch ({}) and its metadata ({}) are not matched",
        persisting_batch_id,
        metadata_id,
    ))]
    NoMatch {
        persisting_batch_id: Uuid,
        metadata_id: Uuid,
    },

    #[snafu(display("Error while insert parquet file and its corresponding processed tombstones in the catalog: {}", source))]
    UpdateCatalog {
        source: iox_catalog::interface::Error,
    },

    #[snafu(display("Error while removing a persisted batch from Data Buffer: {}", source))]
    RemoveBatch { source: crate::data::Error },
}

/// A specialized `Error` for Ingester's Catalog Update errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Update the catalog:
/// The given batch is already persisted, hence, its
/// metadata and applied tomstones should be added in the catalog,
/// and then the batch should be removed from the Ingester's memory
/// NOTE: the caller of this function needs to verify the return error if any
/// to reinvoke it when appropriate
pub async fn update_catalog_after_persisting(
    ingester: &IngestHandlerImpl,
    batch: &Arc<PersistingBatch>,
    tombstones: &[Tombstone],
    metadata: &IoxMetadata,
) -> Result<(ParquetFile, Vec<ProcessedTombstone>)> {
    // verify if the given batch's id match the metadata's id
    if !metadata.match_object_store_id(batch.object_store_id) {
        return Err(Error::NoMatch {
            persisting_batch_id: batch.object_store_id,
            metadata_id: metadata.object_store_id,
        });
    }

    // Insert persisted information into Postgres
    let parquet_file = metadata.to_parquet_file();
    let (inserted_parquet_file, processed_tombstones) = ingester
        .iox_catalog
        .add_parquet_file_with_tombstones(&parquet_file, tombstones)
        .await
        .context(UpdateCatalogSnafu)?;

    // Catalog update was sucessful, remove the batch from its memory
    let mut data_buffer = ingester.data.lock().expect("mutex poisoned");
    data_buffer
        .remove_persisting_batch(batch)
        .context(RemoveBatchSnafu {})?;

    Ok((inserted_parquet_file, processed_tombstones))
}

#[cfg(test)]
mod tests {
    use crate::{
        catalog_update::update_catalog_after_persisting,
        handler::IngestHandlerImpl,
        test_util::{make_persisting_batch, make_persisting_batch_with_meta},
    };
    use iox_catalog::{
        interface::{KafkaTopic, KafkaTopicId},
        mem::MemCatalog,
    };
    use object_store::ObjectStore;
    use std::sync::Arc;
    use uuid::Uuid;

    fn object_store() -> Arc<ObjectStore> {
        Arc::new(ObjectStore::new_in_memory())
    }

    #[tokio::test]
    async fn test_update_batch_removed() {
        // initialize an ingester server
        let mut ingester = IngestHandlerImpl::new(
            KafkaTopic {
                id: KafkaTopicId::new(1),
                name: "Test_KafkaTopic".to_string(),
            },
            vec![],
            Arc::new(MemCatalog::new()),
            object_store(),
        );

        // Make a persisting batch, tombstones and thier metadata after compaction
        let (batch, tombstones, meta) = make_persisting_batch_with_meta().await;

        // Request persiting
        ingester.add_persisting_batch(Arc::clone(&batch));

        // Update the catalog and remove the batch from the buffer
        let (parquet_file, processed_tombstones) =
            update_catalog_after_persisting(&ingester, &batch, &tombstones, &meta)
                .await
                .unwrap();

        // verify the parquet file is actually in the catalog
        assert!(ingester
            .iox_catalog
            .parquet_files()
            .exist(parquet_file.id)
            .await
            .unwrap());

        // verify the processed tombstones are actually in the catalog
        for processed in processed_tombstones {
            assert_eq!(processed.parquet_file_id, parquet_file.id);
            assert!(ingester
                .iox_catalog
                .processed_tombstones()
                .exist(processed.parquet_file_id, processed.tombstone_id)
                .await
                .unwrap());
        }

        // Verfiy the persisting batch is already removed
        assert!(!ingester.is_persisting());
    }

    #[tokio::test]
    async fn test_update_nothing_removed() {
        // initialize an ingester server
        let mut ingester = IngestHandlerImpl::new(
            KafkaTopic {
                id: KafkaTopicId::new(1),
                name: "Test_KafkaTopic".to_string(),
            },
            vec![],
            Arc::new(MemCatalog::new()),
            object_store(),
        );

        // Make a persisting batch, tombstones and thier metadata after compaction
        let (batch, tombstones, meta) = make_persisting_batch_with_meta().await;

        // Request persiting
        ingester.add_persisting_batch(Arc::clone(&batch));

        // create different batch
        let other_batch =
            make_persisting_batch(1, 2, 3, "test_table", 4, Uuid::new_v4(), vec![], vec![]);

        // Get counts before updating
        let before_pf_count = ingester.iox_catalog.parquet_files().count().await.unwrap();
        let before_pt_count = ingester
            .iox_catalog
            .processed_tombstones()
            .count()
            .await
            .unwrap();

        // Update the catalog and remove the batch from the buffer
        let err = update_catalog_after_persisting(&ingester, &other_batch, &tombstones, &meta)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("The Object Store ID of the Persisting Batch"));

        // Verfiy the persisting batch is still there
        assert!(ingester.is_persisting());

        // Verify that the catalog's parquet_files and tombstones are unchanged
        let after_pf_count = ingester.iox_catalog.parquet_files().count().await.unwrap();
        let after_pt_count = ingester
            .iox_catalog
            .processed_tombstones()
            .count()
            .await
            .unwrap();
        assert_eq!(before_pf_count, after_pf_count);
        assert_eq!(before_pt_count, after_pt_count);
    }
}
