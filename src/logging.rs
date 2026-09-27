use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

/// Log to `<dir>/logs/<name>.YYYY-MM-DD.log` (7 days kept) and optionally stderr.
/// Keep the returned guard alive for the life of the process.
pub fn init(dir: &Path, name: &str, stderr: bool) -> Option<WorkerGuard> {
    let filter = || {
        EnvFilter::try_from_env("NYA_LOG").unwrap_or_else(|_| {
            let level = crate::config::ServerConfig::load_or_create(dir).map(|c| c.log_level).unwrap_or_else(|_| "info".into());
            EnvFilter::new(level)
        })
    };
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(name)
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir.join("logs"));
    let (file_layer, guard) = match appender {
        Ok(a) => {
            let (nb, guard) = tracing_appender::non_blocking(a);
            (Some(fmt::layer().with_writer(nb).with_ansi(false)), Some(guard))
        }
        Err(_) => (None, None),
    };
    let stderr_layer = stderr.then(|| fmt::layer().with_writer(std::io::stderr));
    let _ = tracing_subscriber::registry().with(filter()).with(file_layer).with(stderr_layer).try_init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
        eprintln!("panic: {info}");
    }));
    guard
}
