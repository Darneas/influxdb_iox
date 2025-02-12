//! Entrypoint of InfluxDB IOx binary
#![recursion_limit = "512"] // required for print_cpu
#![deny(rustdoc::broken_intra_doc_links, rustdoc::bare_urls, rust_2018_idioms)]
#![warn(
    missing_debug_implementations,
    clippy::explicit_iter_loop,
    clippy::use_self,
    clippy::clone_on_ref_ptr,
    clippy::future_not_send
)]

use crate::commands::tracing::{init_logs_and_tracing, init_simple_logs, TroggingGuard};
use dotenv::dotenv;
use influxdb_iox_client::connection::Builder;
use observability_deps::tracing::warn;
use once_cell::sync::Lazy;
use std::str::FromStr;
use tokio::runtime::Runtime;

mod commands {
    pub mod catalog;
    pub mod database;
    pub mod debug;
    pub mod operations;
    pub mod router;
    pub mod run;
    pub mod server;
    pub mod server_remote;
    pub mod sql;
    pub mod storage;
    pub mod tracing;
}

mod clap_blocks;

pub mod influxdb_ioxd;

enum ReturnCode {
    Failure = 1,
}

static VERSION_STRING: Lazy<String> = Lazy::new(|| {
    format!(
        "{}, revision {}",
        option_env!("CARGO_PKG_VERSION").unwrap_or("UNKNOWN"),
        option_env!("GIT_HASH").unwrap_or("UNKNOWN")
    )
});

/// A comfy_table style that uses single ASCII lines for all borders with plusses at intersections.
///
/// Example:
///
/// ```
/// +------+--------------------------------------+
/// | Name | UUID                                 |
/// +------+--------------------------------------+
/// | bar  | ccc2b8bc-f25d-4341-9b64-b9cfe50d26de |
/// | foo  | 3317ff2b-bbab-43ae-8c63-f0e9ea2f3bdb |
/// +------+--------------------------------------+
const TABLE_STYLE_SINGLE_LINE_BORDERS: &str = "||--+-++|    ++++++";

#[cfg(all(
    feature = "heappy",
    feature = "jemalloc_replacing_malloc",
    not(feature = "clippy")
))]
compile_error!("heappy and jemalloc_replacing_malloc features are mutually exclusive");

#[derive(Debug, clap::Parser)]
#[clap(
    name = "influxdb_iox",
    version = &VERSION_STRING[..],
    about = "InfluxDB IOx server and command line tools",
    long_about = r#"InfluxDB IOx server and command line tools

Examples:
    # Run the InfluxDB IOx server:
    influxdb_iox run database

    # Run the interactive SQL prompt
    influxdb_iox sql

    # Display all server settings
    influxdb_iox run database --help

    # Run the InfluxDB IOx server with extra verbose logging
    influxdb_iox run database -v

    # Run InfluxDB IOx with full debug logging specified with LOG_FILTER
    LOG_FILTER=debug influxdb_iox run database

Command are generally structured in the form:
    <type of object> <action> <arguments>

For example, a command such as the following shows all actions
    available for database chunks, including get and list.

    influxdb_iox database chunk --help
"#
)]
struct Config {
    /// Log filter short-hand.
    ///
    /// Convenient way to set log severity level filter.
    /// Overrides --log-filter / LOG_FILTER.
    ///
    /// -v   'info'
    ///
    /// -vv  'debug,hyper::proto::h1=info,h2=info'
    ///
    /// -vvv 'trace,hyper::proto::h1=info,h2=info'
    #[clap(
        short = 'v',
        long = "--verbose",
        multiple_occurrences = true,
        takes_value = false,
        parse(from_occurrences)
    )]
    pub log_verbose_count: u8,

    /// gRPC address of IOx server to connect to
    #[clap(
        short,
        long,
        global = true,
        env = "IOX_ADDR",
        default_value = "http://127.0.0.1:8082"
    )]
    host: String,

    /// Additional headers to add to CLI requests
    ///
    /// Values should be key value pairs separated by ':'
    #[clap(long, global = true)]
    header: Vec<KeyValue<http::header::HeaderName, http::HeaderValue>>,

    #[clap(long)]
    /// Set the maximum number of threads to use. Defaults to the number of
    /// cores on the system
    num_threads: Option<usize>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Parser)]
enum Command {
    /// Database-related commands
    Database(commands::database::Config),

    /// Run the InfluxDB IOx server
    // Clippy recommended boxing this variant because it's much larger than the others
    Run(Box<commands::run::Config>),

    /// Router-related commands
    Router(commands::router::Config),

    /// IOx server configuration commands
    Server(commands::server::Config),

    /// Manage long-running IOx operations
    Operation(commands::operations::Config),

    /// Start IOx interactive SQL REPL loop
    Sql(commands::sql::Config),

    /// Various commands for catalog manipulation
    Catalog(commands::catalog::Config),

    /// Interrogate internal database data
    Debug(commands::debug::Config),

    /// Initiate a read request to the gRPC storage service.
    Storage(commands::storage::Config),
}

fn main() -> Result<(), std::io::Error> {
    install_crash_handler(); // attempt to render a useful stacktrace to stderr

    // load all environment variables from .env before doing anything
    load_dotenv();

    let config: Config = clap::Parser::parse();

    let tokio_runtime = get_runtime(config.num_threads)?;
    tokio_runtime.block_on(async move {
        let host = config.host;
        let headers = config.header;
        let log_verbose_count = config.log_verbose_count;

        let connection = || async move {
            let builder = headers.into_iter().fold(Builder::default(), |builder, kv| {
                builder.header(kv.key, kv.value)
            });

            match builder.build(&host).await {
                Ok(connection) => connection,
                Err(e) => {
                    eprintln!("Error connecting to {}: {}", host, e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
        };

        fn handle_init_logs(r: Result<TroggingGuard, trogging::Error>) -> TroggingGuard {
            match r {
                Ok(guard) => guard,
                Err(e) => {
                    eprintln!("Initializing logs failed: {}", e);
                    std::process::exit(ReturnCode::Failure as _);
                }
            }
        }

        match config.command {
            Command::Database(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                let connection = connection().await;
                if let Err(e) = commands::database::command(connection, config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Operation(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                let connection = connection().await;
                if let Err(e) = commands::operations::command(connection, config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Server(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                let connection = connection().await;
                if let Err(e) = commands::server::command(connection, config).await {
                    eprintln!("Server command failed: {}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Router(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                let connection = connection().await;
                if let Err(e) = commands::router::command(connection, config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Run(config) => {
                let _tracing_guard =
                    handle_init_logs(init_logs_and_tracing(log_verbose_count, &config));
                if let Err(e) = commands::run::command(*config).await {
                    eprintln!("Server command failed: {}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Sql(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                let connection = connection().await;
                if let Err(e) = commands::sql::command(connection, config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Storage(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                if let Err(e) = commands::storage::command(config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Catalog(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                if let Err(e) = commands::catalog::command(config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
            Command::Debug(config) => {
                let _tracing_guard = handle_init_logs(init_simple_logs(log_verbose_count));
                if let Err(e) = commands::debug::command(config).await {
                    eprintln!("{}", e);
                    std::process::exit(ReturnCode::Failure as _)
                }
            }
        }
    });

    Ok(())
}

/// Creates the tokio runtime for executing IOx
///
/// if nthreads is none, uses the default scheduler
/// otherwise, creates a scheduler with the number of threads
fn get_runtime(num_threads: Option<usize>) -> Result<Runtime, std::io::Error> {
    // NOTE: no log macros will work here!
    //
    // That means use eprintln!() instead of error!() and so on. The log emitter
    // requires a running tokio runtime and is initialised after this function.

    use tokio::runtime::Builder;
    let kind = std::io::ErrorKind::Other;
    match num_threads {
        None => Runtime::new(),
        Some(num_threads) => {
            println!(
                "Setting number of threads to '{}' per command line request",
                num_threads
            );

            match num_threads {
                0 => {
                    let msg = format!(
                        "Invalid num-threads: '{}' must be greater than zero",
                        num_threads
                    );
                    Err(std::io::Error::new(kind, msg))
                }
                1 => Builder::new_current_thread().enable_all().build(),
                _ => Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(num_threads)
                    .build(),
            }
        }
    }
}

/// Source the .env file before initialising the Config struct - this sets
/// any envs in the file, which the Config struct then uses.
///
/// Precedence is given to existing env variables.
fn load_dotenv() {
    match dotenv() {
        Ok(_) => {}
        Err(dotenv::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            // Ignore this - a missing env file is not an error, defaults will
            // be applied when initialising the Config struct.
        }
        Err(e) => {
            eprintln!("FATAL Error loading config from: {}", e);
            eprintln!("Aborting");
            std::process::exit(1);
        }
    };
}

// Based on ideas from
// https://github.com/servo/servo/blob/f03ddf6c6c6e94e799ab2a3a89660aea4a01da6f/ports/servo/main.rs#L58-L79
fn install_crash_handler() {
    unsafe {
        set_signal_handler(libc::SIGSEGV, signal_handler); // handle segfaults
        set_signal_handler(libc::SIGILL, signal_handler); // handle stack overflow and unsupported CPUs
        set_signal_handler(libc::SIGBUS, signal_handler); // handle invalid memory access
    }
}

unsafe extern "C" fn signal_handler(sig: i32) {
    use backtrace::Backtrace;
    use std::process::abort;
    let name = std::thread::current()
        .name()
        .map(|n| format!(" for thread \"{}\"", n))
        .unwrap_or_else(|| "".to_owned());
    eprintln!(
        "Signal {}, Stack trace{}\n{:?}",
        sig,
        name,
        Backtrace::new()
    );
    abort();
}

// based on https://github.com/adjivas/sig/blob/master/src/lib.rs#L34-L52
unsafe fn set_signal_handler(signal: libc::c_int, handler: unsafe extern "C" fn(libc::c_int)) {
    use libc::{sigaction, sigfillset, sighandler_t};
    let mut sigset = std::mem::zeroed();

    // Block all signals during the handler. This is the expected behavior, but
    // it's not guaranteed by `signal()`.
    if sigfillset(&mut sigset) != -1 {
        // Done because sigaction has private members.
        // This is safe because sa_restorer and sa_handlers are pointers that
        // might be null (that is, zero).
        let mut action: sigaction = std::mem::zeroed();

        // action.sa_flags = 0;
        action.sa_mask = sigset;
        action.sa_sigaction = handler as sighandler_t;

        sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// A ':' separated key value pair
#[derive(Debug, Clone)]
struct KeyValue<K, V> {
    pub key: K,
    pub value: V,
}

impl<K, V> std::str::FromStr for KeyValue<K, V>
where
    K: FromStr,
    V: FromStr,
    K::Err: std::fmt::Display,
    V::Err: std::fmt::Display,
{
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use itertools::Itertools;
        match s.splitn(2, ':').collect_tuple() {
            Some((key, value)) => {
                let key = K::from_str(key).map_err(|e| e.to_string())?;
                let value = V::from_str(value).map_err(|e| e.to_string())?;
                Ok(Self { key, value })
            }
            None => Err(format!(
                "Invalid key value pair - expected 'KEY:VALUE' got '{}'",
                s
            )),
        }
    }
}
