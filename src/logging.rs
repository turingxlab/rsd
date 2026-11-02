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
use crate::network::http::Api;
use anyhow::Context;
use flate2::{Compression, write::GzEncoder};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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

// 异步任务被取消后，阻塞打包任务在下一次读取时停止。
struct ArchiveCancellation(Arc<AtomicBool>);

impl Drop for ArchiveCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct ArchiveReader<'a> {
    file: std::io::Take<std::fs::File>,
    cancelled: &'a AtomicBool,
}

impl Read for ArchiveReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_archive_cancelled(self.cancelled)?;
        self.file.read(buffer)
    }
}

fn check_archive_cancelled(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(io::Error::other("Log archive cancelled"));
    }
    Ok(())
}

pub(crate) async fn upload(config: LoggingConfig) -> anyhow::Result<()> {
    let directory = log_directory(&config, dirs::home_dir)?;
    let cancellation = ArchiveCancellation(Arc::new(AtomicBool::new(false)));
    let cancelled = cancellation.0.clone();
    let archive = tokio::task::spawn_blocking(move || create_archive(&directory, &cancelled))
        .await
        .context("Log archive task failed")??;
    let Some((temporary, path)) = archive else {
        tracing::info!("Skipped log upload: log directory is missing or has no files");
        return Ok(());
    };
    // 临时目录必须保留到 HTTP 上传结束，取消或失败时同样自动清理。
    Api::upload_logs(&path).await?;
    drop(temporary);
    tracing::info!("Uploaded log archive");
    Ok(())
}

fn create_archive(
    directory: &Path,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<(tempfile::TempDir, PathBuf)>> {
    match std::fs::metadata(directory) {
        Ok(metadata) => anyhow::ensure!(metadata.is_dir(), "Log path is not a directory"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Failed to inspect log directory"),
    }
    let temporary = tempfile::tempdir().context("Failed to create log archive directory")?;
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    let path = temporary
        .path()
        .join(format!("rsd-logs-{timestamp}.tar.gz"));
    let file = std::fs::File::create(&path).context("Failed to create log archive")?;
    let mut archive = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    let count = append_log_files(
        &mut archive,
        directory,
        directory,
        temporary.path(),
        cancelled,
    )?;
    archive
        .into_inner()
        .context("Failed to finish log tar archive")?
        .finish()
        .context("Failed to finish log gzip archive")?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some((temporary, path)))
}

fn append_log_files(
    archive: &mut tar::Builder<GzEncoder<std::fs::File>>,
    root: &Path,
    directory: &Path,
    temporary: &Path,
    cancelled: &AtomicBool,
) -> anyhow::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("Failed to read log directory: {}", directory.display()))?
    {
        check_archive_cancelled(cancelled)?;
        let entry = entry.context("Failed to read log directory entry")?;
        let path = entry.path();
        let kind = entry.file_type().context("Failed to inspect log entry")?;
        if kind.is_dir() {
            if path != temporary {
                count += append_log_files(archive, root, &path, temporary, cancelled)?;
            }
        } else if kind.is_file() {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("Failed to open log file: {}", path.display()))?;
            let metadata = file.metadata().context("Failed to inspect log file")?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            // 固定读取打开时的长度，避免持续追加的日志导致打包无法结束。
            let reader = ArchiveReader {
                file: file.take(metadata.len()),
                cancelled,
            };
            archive
                .append_data(&mut header, path.strip_prefix(root)?, reader)
                .with_context(|| format!("Failed to archive log file: {}", path.display()))?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn archive_preserves_nested_files_and_skips_empty_or_missing_directories() {
        let directory = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        assert!(
            create_archive(&directory.path().join("missing"), &cancelled)
                .unwrap()
                .is_none()
        );
        assert!(
            create_archive(directory.path(), &cancelled)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        std::fs::write(directory.path().join("rsd.log"), b"current logs").unwrap();
        std::fs::write(directory.path().join("nested/other.txt"), b"nested logs").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("rsd.log", directory.path().join("link")).unwrap();
            std::os::unix::fs::symlink("nested", directory.path().join("linked-dir")).unwrap();
        }
        let (temporary, path) = create_archive(directory.path(), &cancelled)
            .unwrap()
            .unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            std::fs::File::open(&path).unwrap(),
        ));
        let mut entries = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            entries.insert(path, contents);
        }
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[Path::new("rsd.log")], "current logs");
        assert_eq!(entries[Path::new("nested/other.txt")], "nested logs");
        drop(archive);
        drop(temporary);
        assert!(!path.exists());
        cancelled.store(true, Ordering::Relaxed);
        assert!(create_archive(directory.path(), &cancelled).is_err());
        assert!(directory.path().join("rsd.log").exists());
    }

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
