use crate::system_tables::IoxSystemTable;
use arrow::{
    array::{ArrayRef, StringArray, Time64NanosecondArray, TimestampNanosecondArray},
    datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit},
    error::Result,
    record_batch::RecordBatch,
};
use data_types::{error::ErrorLogger, job::Job};
use itertools::Itertools;
use job_registry::JobRegistry;
use std::sync::Arc;
use tracker::TaskTracker;

/// Implementation of system.operations table
#[derive(Debug)]
pub(super) struct OperationsTable {
    schema: SchemaRef,
    db_name: String,
    jobs: Arc<JobRegistry>,
}

impl OperationsTable {
    pub(super) fn new(db_name: String, jobs: Arc<JobRegistry>) -> Self {
        Self {
            schema: operations_schema(),
            db_name,
            jobs,
        }
    }
}

impl IoxSystemTable for OperationsTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn batch(&self) -> Result<RecordBatch> {
        from_task_trackers(self.schema(), &self.db_name, self.jobs.tracked())
            .log_if_error("system.operations table")
    }
}

fn operations_schema() -> SchemaRef {
    let ts = DataType::Time64(TimeUnit::Nanosecond);
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new(
            "start_time",
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            false,
        ),
        Field::new("cpu_time_used", ts.clone(), true),
        Field::new("wall_time_used", ts, true),
        Field::new("table_name", DataType::Utf8, true),
        Field::new("partition_key", DataType::Utf8, true),
        Field::new("chunk_ids", DataType::Utf8, true),
        Field::new("description", DataType::Utf8, true),
    ]))
}

fn from_task_trackers(
    schema: SchemaRef,
    db_name: &str,
    jobs: Vec<TaskTracker<Job>>,
) -> Result<RecordBatch> {
    let jobs = jobs
        .into_iter()
        .filter(|job| {
            job.metadata()
                .db_name()
                .map(|x| x.as_ref() == db_name)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    let ids = jobs
        .iter()
        .map(|job| Some(job.id().to_string()))
        .collect::<StringArray>();
    let statuses = jobs
        .iter()
        .map(|job| {
            let status = job.get_status();
            match status.result() {
                Some(result) => Some(result.name()),
                None => Some(status.name()),
            }
        })
        .collect::<StringArray>();
    let start_time = jobs
        .iter()
        .map(|job| Some(job.start_time().timestamp_nanos()))
        .collect::<TimestampNanosecondArray>();
    let cpu_time_used = jobs
        .iter()
        .map(|job| job.get_status().cpu_nanos().map(|n| n as i64))
        .collect::<Time64NanosecondArray>();
    let wall_time_used = jobs
        .iter()
        .map(|job| job.get_status().wall_nanos().map(|n| n as i64))
        .collect::<Time64NanosecondArray>();
    let partition_keys = jobs
        .iter()
        .map(|job| job.metadata().partition_key())
        .collect::<StringArray>();
    let table_names = jobs
        .iter()
        .map(|job| job.metadata().table_name())
        .collect::<StringArray>();
    let chunk_ids = jobs
        .iter()
        .map(|job| {
            job.metadata()
                .chunk_ids()
                .map(|ids| ids.into_iter().map(|id| id.get()).join(", "))
        })
        .collect::<StringArray>();
    let descriptions = jobs
        .iter()
        .map(|job| Some(job.metadata().description()))
        .collect::<StringArray>();

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(ids) as ArrayRef,
            Arc::new(statuses),
            Arc::new(start_time),
            Arc::new(cpu_time_used),
            Arc::new(wall_time_used),
            Arc::new(table_names),
            Arc::new(partition_keys),
            Arc::new(chunk_ids),
            Arc::new(descriptions),
        ],
    )
}
