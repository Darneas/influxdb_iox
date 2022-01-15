use std::{convert::TryInto, num::TryFromIntError};

use generated_types::influxdata::iox::preserved_catalog::v1 as proto;
use iox_object_store::{ParquetFilePath, ParquetFilePathParseError};
use object_store::path::{parsed::DirsAndFileName, parts::PathPart};
use snafu::{OptionExt, ResultExt, Snafu};
use time::Time;
use uuid::Uuid;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Cannot parse UUID: {}", source))]
    UuidParse { source: uuid::Error },

    #[snafu(display("UUID required but not provided"))]
    UuidRequired {},

    #[snafu(display("Invalid parquet file path: {}", source))]
    InvalidParquetFilePath { source: ParquetFilePathParseError },

    #[snafu(display("Datetime required but missing in serialized catalog"))]
    DateTimeRequired {},

    #[snafu(display("Cannot parse datetime in serialized catalog: {}", source))]
    DateTimeParseError { source: TryFromIntError },

    #[snafu(display(
        "Cannot parse encoding in serialized catalog: {} is not a valid, specified variant",
        data
    ))]
    EncodingParseError { data: i32 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Parse big-endian UUID from protobuf.
pub fn parse_uuid(bytes: &[u8]) -> Result<Option<Uuid>> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        let uuid = Uuid::from_slice(bytes).context(UuidParseSnafu {})?;
        Ok(Some(uuid))
    }
}

/// Parse big-endian UUID from protobuf and fail if protobuf did not provide data.
pub fn parse_uuid_required(bytes: &[u8]) -> Result<Uuid> {
    parse_uuid(bytes)?.context(UuidRequiredSnafu {})
}

/// Parse [`ParquetFilePath`](iox_object_store::ParquetFilePath) from protobuf.
pub fn parse_dirs_and_filename(proto: &proto::Path) -> Result<ParquetFilePath> {
    let dirs_and_file_name = DirsAndFileName {
        directories: proto
            .directories
            .iter()
            .map(|s| PathPart::from(&s[..]))
            .collect(),
        file_name: Some(PathPart::from(&proto.file_name[..])),
    };

    ParquetFilePath::from_relative_dirs_and_file_name(&dirs_and_file_name)
        .context(InvalidParquetFilePathSnafu)
}

/// Store [`ParquetFilePath`](iox_object_store::ParquetFilePath) as protobuf.
pub fn unparse_dirs_and_filename(path: &ParquetFilePath) -> proto::Path {
    let path = path.relative_dirs_and_file_name();
    proto::Path {
        directories: path
            .directories
            .iter()
            .map(|part| part.encoded().to_string())
            .collect(),
        file_name: path
            .file_name
            .as_ref()
            .map(|part| part.encoded().to_string())
            .unwrap_or_default(),
    }
}

/// Parse timestamp from protobuf.
pub fn parse_timestamp(ts: &Option<generated_types::google::protobuf::Timestamp>) -> Result<Time> {
    let ts: generated_types::google::protobuf::Timestamp =
        ts.as_ref().context(DateTimeRequiredSnafu)?.clone();
    let ts = ts.try_into().context(DateTimeParseSnafu)?;
    Ok(Time::from_date_time(ts))
}

/// Parse encoding from protobuf.
pub fn parse_encoding(encoding: i32) -> Result<proto::transaction::Encoding> {
    let parsed = proto::transaction::Encoding::from_i32(encoding)
        .context(EncodingParseSnafu { data: encoding })?;
    if parsed == proto::transaction::Encoding::Unspecified {
        Err(Error::EncodingParseError { data: encoding })
    } else {
        Ok(parsed)
    }
}
