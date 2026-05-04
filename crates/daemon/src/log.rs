//! tracing-appender-based file logger with rotation (daily — see plan note).

use crate::paths;
use anyhow::Result;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

pub fn init(foreground: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = paths::state_dir();
    std::fs::create_dir_all(&dir)?;
    let appender = RollingFileAppender::new(Rotation::DAILY, &dir, "daemon.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter_layer = EnvFilter::try_from_env("CLAUDE_COORD_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let registry = tracing_subscriber::registry().with(filter_layer).with(file_layer);

    if foreground {
        let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
        registry.with(stderr_layer).init();
    } else {
        registry.init();
    }

    Ok(guard)
}
