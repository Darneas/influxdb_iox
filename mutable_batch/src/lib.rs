#![deny(rustdoc::broken_intra_doc_links, rustdoc::bare_urls, rust_2018_idioms)]
#![warn(
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    clippy::explicit_iter_loop,
    clippy::future_not_send,
    clippy::use_self,
    clippy::clone_on_ref_ptr
)]

//! A mutable data structure for a collection of writes.
//!
//! Can be viewed as a mutable version of [`RecordBatch`] that remains the exclusive
//! owner of its buffers, permitting mutability. The in-memory layout is similar, however,
//! permitting fast conversion to [`RecordBatch`]
//!

use std::ops::Range;

use arrow::record_batch::RecordBatch;
use hashbrown::HashMap;
use snafu::{OptionExt, ResultExt, Snafu};

use data_types::write_summary::TimestampSummary;
use schema::selection::Selection;
use schema::{builder::SchemaBuilder, Schema, TIME_COLUMN_NAME};

use crate::column::{Column, ColumnData};

pub mod column;
pub mod payload;
pub mod writer;

pub use payload::*;

#[allow(missing_docs)]
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Column error on column {}: {}", column, source))]
    ColumnError {
        column: String,
        source: column::Error,
    },

    #[snafu(display("arrow conversion error: {}", source))]
    ArrowError { source: arrow::error::ArrowError },

    #[snafu(display("Internal error converting schema: {}", source))]
    InternalSchema { source: schema::builder::Error },

    #[snafu(display("Column not found: {}", column))]
    ColumnNotFound { column: String },

    #[snafu(context(false))]
    WriterError { source: writer::Error },
}

/// A specialized `Error` for [`MutableBatch`] errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Represents a mutable batch of rows (a horizontal subset of a table) which
/// can be appended to and converted into an Arrow `RecordBatch`
#[derive(Debug, Default, Clone)]
pub struct MutableBatch {
    /// Map of column name to index in `MutableBatch::columns`
    column_names: HashMap<String, usize>,

    /// Columns contained within this MutableBatch
    columns: Vec<Column>,

    /// The number of rows in this MutableBatch
    row_count: usize,
}

impl MutableBatch {
    /// Create a new empty batch
    pub fn new() -> Self {
        Self {
            column_names: Default::default(),
            columns: Default::default(),
            row_count: 0,
        }
    }

    /// Returns the schema for a given selection
    ///
    /// If Selection::All the returned columns are sorted by name
    pub fn schema(&self, selection: Selection<'_>) -> Result<Schema> {
        let mut schema_builder = SchemaBuilder::new();
        let schema = match selection {
            Selection::All => {
                for (column_name, column_idx) in self.column_names.iter() {
                    let column = &self.columns[*column_idx];
                    schema_builder.influx_column(column_name, column.influx_type());
                }

                schema_builder
                    .build()
                    .context(InternalSchemaSnafu)?
                    .sort_fields_by_name()
            }
            Selection::Some(cols) => {
                for col in cols {
                    let column = self.column(col)?;
                    schema_builder.influx_column(col, column.influx_type());
                }
                schema_builder.build().context(InternalSchemaSnafu)?
            }
        };

        Ok(schema)
    }

    /// Convert all the data in this `MutableBatch` into a `RecordBatch`
    pub fn to_arrow(&self, selection: Selection<'_>) -> Result<RecordBatch> {
        let schema = self.schema(selection)?;
        let columns = schema
            .iter()
            .map(|(_, field)| {
                let column = self
                    .column(field.name())
                    .expect("schema contains non-existent column");

                column.to_arrow().context(ColumnSnafu {
                    column: field.name(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        RecordBatch::try_new(schema.into(), columns).context(ArrowSnafu {})
    }

    /// Returns an iterator over the columns in this batch in no particular order
    pub fn columns(&self) -> impl Iterator<Item = (&String, &Column)> + '_ {
        self.column_names
            .iter()
            .map(move |(name, idx)| (name, &self.columns[*idx]))
    }

    /// Return the number of rows in this chunk
    pub fn rows(&self) -> usize {
        self.row_count
    }

    /// Returns a summary of the write timestamps in this chunk if a
    /// time column exists
    pub fn timestamp_summary(&self) -> Option<TimestampSummary> {
        let time = self.column_names.get(TIME_COLUMN_NAME)?;
        let mut summary = TimestampSummary::default();
        match &self.columns[*time].data {
            ColumnData::I64(col_data, _) => {
                for t in col_data {
                    summary.record_nanos(*t)
                }
            }
            _ => unreachable!(),
        }
        Some(summary)
    }

    /// Extend this [`MutableBatch`] with the contents of `other`
    pub fn extend_from(&mut self, other: &Self) -> Result<()> {
        let mut writer = writer::Writer::new(self, other.row_count);
        writer.write_batch(other)?;
        writer.commit();
        Ok(())
    }

    /// Extend this [`MutableBatch`] with `range` rows from `other`
    pub fn extend_from_range(&mut self, other: &Self, range: Range<usize>) -> Result<()> {
        let mut writer = writer::Writer::new(self, range.end - range.start);
        writer.write_batch_range(other, range)?;
        writer.commit();
        Ok(())
    }

    /// Extend this [`MutableBatch`] with `ranges` rows from `other`
    pub fn extend_from_ranges(&mut self, other: &Self, ranges: &[Range<usize>]) -> Result<()> {
        let to_insert = ranges.iter().map(|x| x.end - x.start).sum();

        let mut writer = writer::Writer::new(self, to_insert);
        writer.write_batch_ranges(other, ranges)?;
        writer.commit();
        Ok(())
    }

    /// Returns a reference to the specified column
    pub fn column(&self, column: &str) -> Result<&Column> {
        let idx = self
            .column_names
            .get(column)
            .context(ColumnNotFoundSnafu { column })?;

        Ok(&self.columns[*idx])
    }

    /// Return the approximate memory size of the batch, in bytes.
    ///
    /// This includes `Self`.
    pub fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .column_names
                .iter()
                .map(|(k, v)| std::mem::size_of_val(k) + k.capacity() + std::mem::size_of_val(v))
                .sum::<usize>()
            + self.columns.iter().map(|c| c.size()).sum::<usize>()
    }
}
