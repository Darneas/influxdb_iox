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

//! A mutable data structure for a collection of writes
//!
//! Currently supports:
//! - `[TableBatch`] writes
//! - [`RecordBatch`] conversion

use crate::column::Column;
use arrow::record_batch::RecordBatch;
use entry::TableBatch;
use hashbrown::HashMap;
use schema::selection::Selection;
use schema::{builder::SchemaBuilder, Schema};
use snafu::{ensure, OptionExt, ResultExt, Snafu};

pub mod column;

#[allow(missing_docs)]
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Column error on column {}: {}", column, source))]
    ColumnError {
        column: String,
        source: column::Error,
    },

    #[snafu(display("Column {} had {} rows, expected {}", column, expected, actual))]
    IncorrectRowCount {
        column: String,
        expected: usize,
        actual: usize,
    },

    #[snafu(display("arrow conversion error: {}", source))]
    ArrowError { source: arrow::error::ArrowError },

    #[snafu(display("Internal error converting schema: {}", source))]
    InternalSchema { source: schema::builder::Error },

    #[snafu(display("Column not found: {}", column))]
    ColumnNotFound { column: String },

    #[snafu(display("Mask had {} rows, expected {}", expected, actual))]
    IncorrectMaskLength { expected: usize, actual: usize },
}

/// A specialized `Error` for [`MutableBatch`] errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Represents a Chunk of data (a horizontal subset of a table) in
/// the mutable store.
#[derive(Debug, Default)]
pub struct MutableBatch {
    /// Map of column id from the chunk dictionary to the column
    columns: HashMap<String, Column>,
}

impl MutableBatch {
    /// Create a new empty batch
    pub fn new() -> Self {
        Self {
            columns: Default::default(),
        }
    }

    /// Write the contents of a [`TableBatch`] into this MutableBatch.
    ///
    /// If `mask` is provided, only entries that are marked w/ `true` are written.
    ///
    /// Panics if the batch specifies a different name for the table in this Chunk
    pub fn write_table_batch(
        &mut self,
        batch: TableBatch<'_>,
        mask: Option<&[bool]>,
    ) -> Result<()> {
        self.write_columns(batch.columns(), mask)
    }

    /// Returns the schema for a given selection
    ///
    /// If Selection::All the returned columns are sorted by name
    pub fn schema(&self, selection: Selection<'_>) -> Result<Schema> {
        let mut schema_builder = SchemaBuilder::new();
        let schema = match selection {
            Selection::All => {
                for (column_name, column) in self.columns.iter() {
                    schema_builder.influx_column(column_name, column.influx_type());
                }

                schema_builder
                    .build()
                    .context(InternalSchema)?
                    .sort_fields_by_name()
            }
            Selection::Some(cols) => {
                for col in cols {
                    let column = self.column(col)?;
                    schema_builder.influx_column(col, column.influx_type());
                }
                schema_builder.build().context(InternalSchema)?
            }
        };

        Ok(schema)
    }

    /// Convert the table specified in this chunk into some number of
    /// record batches, appended to dst
    pub fn to_arrow(&self, selection: Selection<'_>) -> Result<RecordBatch> {
        let schema = self.schema(selection)?;
        let columns = schema
            .iter()
            .map(|(_, field)| {
                let column = self
                    .columns
                    .get(field.name())
                    .expect("schema contains non-existent column");

                column.to_arrow().context(ColumnError {
                    column: field.name(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        RecordBatch::try_new(schema.into(), columns).context(ArrowError {})
    }

    /// Returns an iterator over the columns in this batch in no particular order
    pub fn columns(&self) -> impl Iterator<Item = (&String, &Column)> + '_ {
        self.columns.iter()
    }

    /// Return the number of rows in this chunk
    pub fn rows(&self) -> usize {
        self.columns
            .values()
            .next()
            .map(|col| col.len())
            .unwrap_or(0)
    }

    /// Returns a reference to the specified column
    pub(crate) fn column(&self, column: &str) -> Result<&Column> {
        self.columns.get(column).context(ColumnNotFound { column })
    }

    /// Validates the schema of the passed in columns, then adds their values to
    /// the associated columns in the table and updates summary statistics.
    ///
    /// If `mask` is provided, only entries that are marked w/ `true` are written.
    fn write_columns(
        &mut self,
        columns: Vec<entry::Column<'_>>,
        mask: Option<&[bool]>,
    ) -> Result<()> {
        let row_count_before_insert = self.rows();
        let additional_rows = columns.first().map(|x| x.row_count).unwrap_or_default();
        let masked_values = if let Some(mask) = mask {
            ensure!(
                additional_rows == mask.len(),
                IncorrectMaskLength {
                    expected: additional_rows,
                    actual: mask.len(),
                }
            );
            mask.iter().filter(|x| !*x).count()
        } else {
            0
        };
        let final_row_count = row_count_before_insert + additional_rows - masked_values;

        // get the column ids and validate schema for those that already exist
        columns.iter().try_for_each(|column| {
            ensure!(
                column.row_count == additional_rows,
                IncorrectRowCount {
                    column: column.name(),
                    expected: additional_rows,
                    actual: column.row_count,
                }
            );

            if let Some(c) = self.columns.get(column.name()) {
                c.validate_schema(column).context(ColumnError {
                    column: column.name(),
                })?;
            }

            Ok(())
        })?;

        for fb_column in columns {
            let influx_type = fb_column.influx_type();

            let column = self
                .columns
                .raw_entry_mut()
                .from_key(fb_column.name())
                .or_insert_with(|| {
                    (
                        fb_column.name().to_string(),
                        Column::new(row_count_before_insert, influx_type),
                    )
                })
                .1;

            column.append(&fb_column, mask).context(ColumnError {
                column: fb_column.name(),
            })?;

            assert_eq!(column.len(), final_row_count);
        }

        // Pad any columns that did not have values in this batch with NULLs
        for c in self.columns.values_mut() {
            c.push_nulls_to_len(final_row_count);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entry::test_helpers::lp_to_entry;
    use schema::{InfluxColumnType, InfluxFieldType};

    #[test]
    fn write_columns_validates_schema() {
        let mut table = MutableBatch::new();
        let lp = "foo,t1=asdf iv=1i,uv=1u,fv=1.0,bv=true,sv=\"hi\" 1";
        let entry = lp_to_entry(lp);
        table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .unwrap();

        let lp = "foo t1=\"string\" 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        existing: InfluxColumnType::Tag,
                        inserted: InfluxColumnType::Field(InfluxFieldType::String)
                    }
                } if column == "t1"
            ),
            "didn't match returned error: {:?}",
            response
        );

        let lp = "foo iv=1u 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        inserted: InfluxColumnType::Field(InfluxFieldType::UInteger),
                        existing: InfluxColumnType::Field(InfluxFieldType::Integer)
                    }
                } if column == "iv"
            ),
            "didn't match returned error: {:?}",
            response
        );

        let lp = "foo fv=1i 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        existing: InfluxColumnType::Field(InfluxFieldType::Float),
                        inserted: InfluxColumnType::Field(InfluxFieldType::Integer)
                    }
                } if column == "fv"
            ),
            "didn't match returned error: {:?}",
            response
        );

        let lp = "foo bv=1 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        existing: InfluxColumnType::Field(InfluxFieldType::Boolean),
                        inserted: InfluxColumnType::Field(InfluxFieldType::Float)
                    }
                } if column == "bv"
            ),
            "didn't match returned error: {:?}",
            response
        );

        let lp = "foo sv=true 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        existing: InfluxColumnType::Field(InfluxFieldType::String),
                        inserted: InfluxColumnType::Field(InfluxFieldType::Boolean),
                    }
                } if column == "sv"
            ),
            "didn't match returned error: {:?}",
            response
        );

        let lp = "foo,sv=\"bar\" f=3i 1";
        let entry = lp_to_entry(lp);
        let response = table
            .write_columns(
                entry
                    .partition_writes()
                    .unwrap()
                    .first()
                    .unwrap()
                    .table_batches()
                    .first()
                    .unwrap()
                    .columns(),
                None,
            )
            .err()
            .unwrap();
        assert!(
            matches!(
                &response,
                Error::ColumnError {
                    column,
                    source: column::Error::TypeMismatch {
                        existing: InfluxColumnType::Field(InfluxFieldType::String),
                        inserted: InfluxColumnType::Tag,
                    }
                } if column == "sv"
            ),
            "didn't match returned error: {:?}",
            response
        );
    }
}
