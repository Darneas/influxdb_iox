use crate::common::server_fixture::{ServerFixture, ServerType, TestConfig};
use tempfile::TempDir;

#[tokio::test]
async fn querying_without_data_returns_nothing() {
    let write_buffer_dir = TempDir::new().unwrap();

    let test_config = TestConfig::new(ServerType::Ingester)
        .with_env("INFLUXDB_IOX_CATALOG_DSN", "mem")
        .with_env("INFLUXDB_IOX_WRITE_BUFFER_TYPE", "file")
        .with_env(
            "INFLUXDB_IOX_WRITE_BUFFER_ADDR",
            write_buffer_dir.path().display().to_string(),
        )
        .with_env("INFLUXDB_IOX_WRITE_BUFFER_PARTITION_RANGE_START", "1")
        .with_env("INFLUXDB_IOX_WRITE_BUFFER_PARTITION_RANGE_END", "2");
    let server = ServerFixture::create_single_use_with_config(test_config).await;

    let mut ingester_flight = server.ingester_flight_client();

    // This does nothing except test the client handshake implementation.
    ingester_flight.handshake().await.unwrap();
}
