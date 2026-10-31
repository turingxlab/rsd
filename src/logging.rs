// Copyright 2026 The rsd Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config::{LogOutput, LogRotation, LoggingConfig};
use anyhow::Context;
use std::io::{self, Write};
use std::path::PathBuf;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::time::LocalTime;

pub(crate) struct Logging {
    writer: Option<NonBlocking>,
    guard: Option<WorkerGuard>,
}

impl Logging {
    pub(crate) fn shutdown(&mut self) {
        drop(self.guard.take());
    }
}

impl Write for Logging {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.writer.as_mut() {
            Some(writer) => writer.write(buffer),
            None => io::stdout().lock().write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.writer.as_mut() {
            Some(writer) => writer.flush(),
            None => io::stdout().lock().flush(),
        }
    }
}

pub(crate) fn init(config: &LoggingConfig) -> anyhow::Result<Logging> {
    let timer = LocalTime::new(
        time::format_description::parse_borrowed::<3>(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]",
        )
        .expect("Invalid time format"),
    );
    // RUST_LOG（有效时）> [logging].level > 默认 info。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.level));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer);

    if config.output == LogOutput::File {
        let (writer, guard) = file_writer(config)?;
        subscriber
            .with_ansi(false)
            .with_writer(writer.clone())
            .try_init()
            .map_err(|error| anyhow::anyhow!("Failed to initialize file logging: {error}"))?;
        Ok(Logging {
            writer: Some(writer),
            guard: Some(guard),
        })
    } else {
        subscriber
            .try_init()
            .map_err(|error| anyhow::anyhow!("Failed to initialize console logging: {error}"))?;
        Ok(Logging {
            writer: None,
            guard: None,
        })
    }
}

fn file_writer(config: &LoggingConfig) -> anyhow::Result<(NonBlocking, WorkerGuard)> {
    let directory = log_directory(config, dirs::home_dir)?;
    let rotation = match config.rotation {
        LogRotation::Never => Rotation::NEVER,
        LogRotation::Minutely => Rotation::MINUTELY,
        LogRotation::Hourly => Rotation::HOURLY,
        LogRotation::Daily => Rotation::DAILY,
        LogRotation::Weekly => Rotation::WEEKLY,
    };
    let mut builder = RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix("rsd")
        .filename_suffix("log");
    // 不轮转时无需清理，避免库在重新打开 rsd.log 前将其删除。
    if let Some(max_files) = config
        .max_log_files
        .filter(|_| config.rotation != LogRotation::Never)
    {
        builder = builder.max_log_files(max_files.get());
    }
    let appender = builder.build(&directory).with_context(|| {
        format!(
            "Failed to initialize log directory: {}",
            directory.display()
        )
    })?;
    // 队列满时等待后台写入，避免主动丢弃日志。
    Ok(NonBlockingBuilder::default().lossy(false).finish(appender))
}

fn log_directory(
    config: &LoggingConfig,
    home_dir: impl FnOnce() -> Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    match &config.directory {
        Some(directory) => {
            anyhow::ensure!(
                !directory.trim().is_empty(),
                "logging.directory must not be empty"
            );
            Ok(PathBuf::from(directory))
        }
        None => Ok(home_dir()
            .context("Failed to resolve home directory; set logging.directory explicitly")?
            .join(".rsd")
            .join("logs")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rsd-logging-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn config(&self) -> LoggingConfig {
            LoggingConfig {
                output: LogOutput::File,
                directory: Some(self.0.join("logs").to_str().unwrap().to_owned()),
                ..LoggingConfig::default()
            }
        }
    }

    #[test]
    fn resolves_default_directory_and_reports_missing_home() {
        let config = LoggingConfig::default();
        let home = std::env::temp_dir().join("rsd-test-home");
        assert_eq!(
            log_directory(&config, || Some(home.clone())).unwrap(),
            home.join(".rsd/logs")
        );
        assert!(log_directory(&config, || None).is_err());
        let config = LoggingConfig {
            directory: Some("custom-logs".to_owned()),
            ..config
        };
        assert_eq!(
            log_directory(&config, || panic!("must not resolve home")).unwrap(),
            PathBuf::from("custom-logs")
        );
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_directory_appends_and_flushes_on_drop() {
        let dir = TestDir::new();
        let config = dir.config();
        for message in ["first message", "second message"] {
            let (writer, guard) = file_writer(&config).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(writer)
                .finish();
            tracing::subscriber::with_default(subscriber, || tracing::info!("{message}"));
            drop(guard);
        }
        let contents = std::fs::read_to_string(dir.0.join("logs/rsd.log")).unwrap();
        assert!(contents.contains("first message"));
        assert!(contents.contains("second message"));
        assert!(!contents.contains('\u{1b}'));
    }

    #[test]
    fn banner_precedes_logs_and_bypasses_filter() {
        for filter in ["info", "off"] {
            let dir = TestDir::new();
            let (writer, guard) = file_writer(&dir.config()).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer.clone())
                .finish();
            let mut logging = Logging {
                writer: Some(writer),
                guard: Some(guard),
            };
            tracing::subscriber::with_default(subscriber, || {
                crate::banner::print_banner(&mut logging).unwrap();
                tracing::info!("Application running.");
            });
            logging.shutdown();
            let contents = std::fs::read_to_string(dir.0.join("logs/rsd.log")).unwrap();
            let mut banner = Vec::new();
            crate::banner::print_banner(&mut banner).unwrap();
            let banner = String::from_utf8(banner).unwrap();
            assert!(contents.starts_with(&banner));
            if filter == "off" {
                assert_eq!(contents, banner);
            } else {
                assert!(contents[banner.len()..].contains("Application running."));
            }
        }
    }

    #[test]
    fn file_in_place_of_directory_returns_path_error() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join("logs"), b"not a directory").unwrap();
        let error = file_writer(&dir.config()).err().expect("must fail");
        assert!(error.to_string().contains("logs"));
    }

    #[test]
    fn never_rotation_preserves_existing_file_with_limit() {
        let dir = TestDir::new();
        let mut config = dir.config();
        config.max_log_files = std::num::NonZeroUsize::new(1);
        std::fs::create_dir_all(dir.0.join("logs")).unwrap();
        let path = dir.0.join("logs/rsd.log");
        std::fs::write(&path, b"existing logs").unwrap();
        let (_, guard) = file_writer(&config).unwrap();
        drop(guard);
        assert_eq!(std::fs::read(path).unwrap(), b"existing logs");
    }

    #[test]
    fn rolling_appender_limits_files_and_keeps_unrelated_files() {
        let dir = TestDir::new();
        let mut config = dir.config();
        config.rotation = LogRotation::Daily;
        config.max_log_files = std::num::NonZeroUsize::new(2);
        let logs = dir.0.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        for name in ["rsd.2020-01-01.log", "rsd.2020-01-02.log", "other.txt"] {
            std::fs::write(logs.join(name), b"old logs").unwrap();
        }
        let (_, guard) = file_writer(&config).unwrap();
        drop(guard);
        assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 3);
        assert!(logs.join("other.txt").exists());
        let today = time::OffsetDateTime::now_utc().date();
        assert!(logs.join(format!("rsd.{today}.log")).exists());
    }

    #[test]
    fn creates_file_for_each_rotation_period() {
        for rotation in [
            LogRotation::Minutely,
            LogRotation::Hourly,
            LogRotation::Daily,
            LogRotation::Weekly,
        ] {
            let dir = TestDir::new();
            let mut config = dir.config();
            config.rotation = rotation;
            let before = time::OffsetDateTime::now_utc().date();
            let (_, guard) = file_writer(&config).unwrap();
            drop(guard);
            let after = time::OffsetDateTime::now_utc().date();
            let names: Vec<_> = std::fs::read_dir(dir.0.join("logs"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names.len(), 1);
            let name = &names[0];
            assert!(name.starts_with("rsd.") && name.ends_with(".log"));
            let expected_len = match rotation {
                LogRotation::Minutely => 24,
                LogRotation::Hourly => 21,
                LogRotation::Daily | LogRotation::Weekly => 18,
                LogRotation::Never => unreachable!(),
            };
            assert_eq!(name.len(), expected_len);
            if rotation == LogRotation::Weekly {
                let week_filename = |date: time::Date| {
                    let sunday = date
                        - time::Duration::days(date.weekday().number_days_from_sunday().into());
                    format!("rsd.{sunday}.log")
                };
                assert!(name == &week_filename(before) || name == &week_filename(after));
            }
        }
    }
}
