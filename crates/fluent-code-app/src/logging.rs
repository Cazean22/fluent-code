use std::fs;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::prelude::*;

use crate::config::Config;
use crate::error::{FluentCodeError, Result};

#[derive(Debug)]
pub struct LoggingGuard {
    _file_guard: Option<WorkerGuard>,
}

pub fn init_logging(config: &Config) -> Result<LoggingGuard> {
    let file_filter = if config.logging.file.enabled {
        Some(parse_filter(
            "logging.file.level",
            &config.logging.file.level,
        )?)
    } else {
        None
    };
    let stderr_filter = if config.logging.stderr.enabled {
        Some(parse_filter(
            "logging.stderr.level",
            &config.logging.stderr.level,
        )?)
    } else {
        None
    };

    let (file_writer, file_guard) = if config.logging.file.enabled {
        Some(create_file_writer(&config.logging.file.path)?)
    } else {
        None
    }
    .map_or((None, None), |(writer, guard)| (Some(writer), Some(guard)));

    let file_layer = file_writer.zip(file_filter).map(|(writer, filter)| {
        tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(filter)
    });
    let stderr_layer = stderr_filter.map(|filter| {
        tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(false)
            .with_target(false)
            .with_writer(std::io::stderr)
            .with_filter(filter)
    });

    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer);

    tracing::subscriber::set_global_default(subscriber).map_err(|error| {
        FluentCodeError::Config(format!("failed to initialize logging subscriber: {error}"))
    })?;

    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

fn create_file_writer(
    path: &Path,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let parent = path.parent().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "logging.file.path '{}' must include a parent directory",
            path.display()
        ))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "logging.file.path '{}' must include a file name",
            path.display()
        ))
    })?;
    let file_name = file_name.to_str().ok_or_else(|| {
        FluentCodeError::Config(format!(
            "logging.file.path '{}' must be valid UTF-8",
            path.display()
        ))
    })?;

    fs::create_dir_all(parent)?;

    let appender = tracing_appender::rolling::never(parent, file_name);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Ok((writer, guard))
}

fn parse_filter(setting_name: &str, level: &str) -> Result<EnvFilter> {
    EnvFilter::try_new(level).map_err(|error| {
        FluentCodeError::Config(format!("invalid {setting_name} value '{level}': {error}"))
    })
}

pub fn config_source_for_log(config_path: Option<&Path>) -> String {
    config_path
        .map(path_for_log)
        .unwrap_or_else(|| "defaults".to_string())
}

pub fn path_for_log(path: &Path) -> String {
    match std::env::current_dir() {
        Ok(current_dir) => path
            .strip_prefix(&current_dir)
            .unwrap_or(path)
            .display()
            .to_string(),
        Err(_) => path.display().to_string(),
    }
}
