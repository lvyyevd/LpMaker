use anyhow::Result;
use tracing_subscriber::prelude::*;

/// Never log private keys, raw signed transactions or HTTP request bodies.
pub fn init(
    c: &crate::config::LoggingConfig,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(&c.directory)?;
    let file = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("lpmaker")
        .filename_suffix("jsonl")
        .max_log_files(c.retained_files)
        .build(&c.directory)?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(file);
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_new(&c.level)?)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::io::stderr),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(writer),
        )
        .try_init()?;
    Ok(guard)
}
