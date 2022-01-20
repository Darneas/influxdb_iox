use influxdb_iox_client::{connection::Connection, management, operations::Client};
use thiserror::Error;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
pub enum Error {
    #[error("Client error: {0}")]
    ClientError(#[from] influxdb_iox_client::error::Error),

    #[error("Output serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Manage long-running IOx operations
#[derive(Debug, clap::Parser)]
pub struct Config {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Parser)]
enum Command {
    /// Get list of running operations
    List,

    /// Get a specific operation
    Get {
        /// The id of the operation
        id: usize,
    },

    /// Wait for a specific operation to complete
    Wait {
        /// The id of the operation
        id: usize,

        /// Maximum number of nanoseconds to wait before returning current
        /// status
        nanos: Option<u64>,
    },

    /// Cancel a specific operation
    Cancel {
        /// The id of the operation
        id: usize,
    },

    /// Spawns a dummy test operation
    Test { nanos: Vec<u64> },
}

pub async fn command(connection: Connection, config: Config) -> Result<()> {
    match config.command {
        Command::List => {
            let operations = Client::new(connection).list_operations().await?;
            serde_json::to_writer_pretty(std::io::stdout(), &operations)?;
        }
        Command::Get { id } => {
            let operation = Client::new(connection).get_operation(id).await?;
            serde_json::to_writer_pretty(std::io::stdout(), &operation)?;
        }
        Command::Wait { id, nanos } => {
            let timeout = nanos.map(std::time::Duration::from_nanos);
            let operation = Client::new(connection).wait_operation(id, timeout).await?;
            serde_json::to_writer_pretty(std::io::stdout(), &operation)?;
        }
        Command::Cancel { id } => {
            Client::new(connection).cancel_operation(id).await?;
            println!("Ok");
        }
        Command::Test { nanos } => {
            let operation = management::Client::new(connection)
                .create_dummy_job(nanos)
                .await?;
            serde_json::to_writer_pretty(std::io::stdout(), &operation)?;
        }
    }

    Ok(())
}
