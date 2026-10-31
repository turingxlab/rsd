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

use crate::network::protocol::{UpdateInfo, WsMessage};
use crate::process::ProcessManager;
use anyhow::{Context, Result, bail};
use self_update::{Checksum, Release, ReleaseAsset, ReleaseSource, VersionStatus};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;
use url::Url;

const SHA256_HEX_LEN: usize = 64;

pub(crate) type UpdateResult = Result<VersionStatus>;
pub(crate) type UpdateReceiver = oneshot::Receiver<UpdateResult>;

/// 自更新任务协调器。主循环只负责分发消息和等待结果，更新生命周期由此处管理。
pub(crate) struct UpdateManager {
    updating: AtomicBool,
}

impl UpdateManager {
    pub(crate) fn new() -> Self {
        Self {
            updating: AtomicBool::new(false),
        }
    }

    /// 处理一次更新消息。返回值表示是否启动了后台更新任务。
    pub(crate) fn dispatch(&self, msg: WsMessage) -> Option<UpdateReceiver> {
        let info = match parse_update_info(msg) {
            Ok(info) => info,
            Err(error) => {
                tracing::error!("{:#}", error);
                return None;
            }
        };

        tracing::info!(
            "Received update info: version={}, target={}, url={}, sha256={}",
            info.version,
            info.target,
            info.url,
            info.sha256
        );

        let current_target = self_update::get_target();
        if info.target != current_target {
            tracing::warn!(
                target = %info.target,
                current_target,
                "Ignoring self-update for a different target"
            );
            return None;
        }

        if self
            .updating
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::warn!("Ignoring update command: another update is already running");
            return None;
        }

        let (sender, receiver) = oneshot::channel();
        tracing::info!(
            "Checking self-update: current_version={}, requested_version={}",
            env!("CARGO_PKG_VERSION"),
            info.version
        );
        tokio::task::spawn_blocking(move || {
            let result = apply_update(&info);
            let _ = sender.send(result);
        });
        Some(receiver)
    }

    /// 处理后台更新结果；更新成功后停止受管进程并重启当前程序。
    pub(crate) fn handle_result(
        &self,
        result: std::result::Result<UpdateResult, oneshot::error::RecvError>,
        process_manager: &mut ProcessManager,
        before_restart: impl FnOnce(),
    ) -> Result<()> {
        // 结果被主循环消费前保持锁定，防止新任务覆盖尚未处理的 receiver。
        self.updating.store(false, Ordering::Release);
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::error!("{:#}", error);
                return Ok(());
            }
        };

        match result {
            Ok(status) if status.is_updated() => {
                tracing::info!(
                    "Updated to v{}; stopping managed applications and restarting",
                    status.version()
                );
                if let Err(error) = process_manager.shutdown() {
                    tracing::warn!(%error, "Failed to stop applications before restart");
                }
                // restart 在 Unix 上直接替换进程，不会运行析构函数。
                before_restart();
                match self_update::restart::restart() {
                    Ok(never) => match never {},
                    Err(error) => {
                        Err(anyhow::Error::new(error)
                            .context("Failed to restart after self-update"))
                    }
                }
            }
            Ok(status) => {
                tracing::info!(
                    "Self-update skipped: no newer version available; current_version={}",
                    status.version()
                );
                Ok(())
            }
            Err(error) => {
                tracing::error!("{:#}", error);
                Ok(())
            }
        }
    }
}

/// 解析并校验服务端下发的更新信息。
pub(crate) fn parse_update_info(msg: WsMessage) -> Result<UpdateInfo> {
    let mut info: UpdateInfo = serde_json::from_value(msg.data.context("Update data is missing")?)
        .context("Failed to parse update data")?;

    if info.version.trim().is_empty() {
        bail!("Update version is empty");
    }

    if info.target.trim().is_empty() {
        bail!("Update target is empty");
    }

    let url = Url::parse(&info.url).context("Update URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Update URL must use http or https");
    }

    if info.sha256.len() != SHA256_HEX_LEN
        || !info.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Update SHA-256 must be 64 hexadecimal characters");
    }
    info.sha256.make_ascii_lowercase();

    Ok(info)
}

/// 执行阻塞式自更新。必须从 `spawn_blocking` 等阻塞线程调用。
pub(crate) fn apply_update(info: &UpdateInfo) -> Result<VersionStatus> {
    let bin_name = env!("CARGO_PKG_NAME");
    let asset_name = download_asset_name(bin_name, std::env::consts::EXE_SUFFIX);
    let release = Release::builder()
        .version(info.version.as_str())
        .asset(ReleaseAsset::new(asset_name, info.url.clone()))
        .build()
        .context("Failed to construct update release")?;

    let expected_sha256 = info.sha256.clone();
    let status = self_update::backends::custom::Update::configure()
        .source(WebSocketReleaseSource { release })
        .bin_name(bin_name)
        .current_version(env!("CARGO_PKG_VERSION"))
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .asset_matcher(|assets| assets.first().cloned())
        .verify_checksum(Checksum::Sha256(info.sha256.clone()))
        .verify_binary(move |path| verify_update_binary(path, &expected_sha256))
        .build()
        .context("Failed to configure self-update")?
        .update()?;

    Ok(status)
}

fn download_asset_name(bin_name: &str, exe_suffix: &str) -> String {
    // 下载和提取发生在同一临时目录，名称必须不同，否则提取会截断源文件。
    format!("{bin_name}{exe_suffix}.tmp")
}

fn verify_update_binary(path: &Path, expected_sha256: &str) -> self_update::Result<()> {
    let mut file = std::fs::File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Err(self_update::errors::Error::verification_rejected(
            "Update binary is empty",
        ));
    }

    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual_sha256: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(self_update::errors::Error::verification_rejected(
            "Update binary SHA-256 mismatch",
        ));
    }
    Ok(())
}

/// 来自 WebSocket 消息的一次性 ReleaseSource。
struct WebSocketReleaseSource {
    release: Release,
}

impl ReleaseSource for WebSocketReleaseSource {
    fn get_releases(&self) -> self_update::Result<Vec<Release>> {
        Ok(vec![self.release.clone()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rsd-update-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha256(contents: &[u8]) -> String {
        Sha256::digest(contents)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn raw_binary_extraction_preserves_download_for_both_platform_names() {
        let contents = b"new rsd binary\0with payload";
        let expected_sha256 = sha256(contents);
        for suffix in ["", ".exe"] {
            let dir = TestDir::new();
            let asset_name = download_asset_name("rsd", suffix);
            assert_eq!(asset_name, format!("rsd{suffix}.tmp"));
            let download = dir.0.join(asset_name);
            let binary_name = format!("rsd{suffix}");
            let extracted = dir.0.join(&binary_name);
            let installed = dir.0.join("installed-rsd");
            std::fs::write(&download, contents).expect("write download");
            std::fs::write(&installed, b"old binary").expect("write old binary");

            self_update::Extract::from_source(&download)
                .extract_file(&dir.0, &binary_name)
                .expect("extract raw binary");
            verify_update_binary(&extracted, &expected_sha256).expect("verify extracted binary");
            self_update::Move::from_source(&extracted)
                .to_dest(&installed)
                .expect("install into temporary destination");

            assert_eq!(std::fs::read(&download).unwrap(), contents);
            assert_eq!(std::fs::read(&installed).unwrap(), contents);
        }
    }

    #[test]
    fn rejects_empty_and_corrupted_binaries_before_install() {
        let dir = TestDir::new();
        let candidate = dir.0.join("candidate");
        let installed = dir.0.join("installed-rsd");
        std::fs::write(&installed, b"old binary").unwrap();
        for (contents, expected_sha256, reason) in [
            (b"".as_slice(), sha256(b""), "empty"),
            (
                b"corrupted".as_slice(),
                sha256(b"expected binary"),
                "SHA-256",
            ),
        ] {
            std::fs::write(&candidate, contents).unwrap();
            let result = verify_update_binary(&candidate, &expected_sha256)
                .and_then(|()| self_update::Move::from_source(&candidate).to_dest(&installed));
            assert!(result.unwrap_err().to_string().contains(reason));
            assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
        }
    }

    #[tokio::test]
    async fn update_stays_locked_until_result_is_consumed_and_allows_retry() {
        let manager = UpdateManager::new();
        let mut processes = ProcessManager::default();
        let message = || {
            update_message(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "target": self_update::get_target(),
                "url": "https://example.com/rsd",
                "sha256": "0".repeat(SHA256_HEX_LEN)
            }))
        };
        // 同版本不下载，但仍经过完整的后台任务和结果交付流程。
        let receiver = manager.dispatch(message()).expect("start update");
        let result = receiver.await;
        assert!(matches!(&result, Ok(Ok(status)) if !status.is_updated()));
        assert!(manager.dispatch(message()).is_none());
        manager
            .handle_result(result, &mut processes, || {})
            .unwrap();

        let receiver = manager.dispatch(message()).expect("retry after result");
        let _ = receiver.await;
        manager
            .handle_result(
                Ok(Err(anyhow::anyhow!("update failed"))),
                &mut processes,
                || {},
            )
            .unwrap();
        assert!(!manager.updating.load(Ordering::Acquire));

        let receiver = manager.dispatch(message()).expect("retry after failure");
        let _ = receiver.await;
        let (sender, receiver) = oneshot::channel::<UpdateResult>();
        drop(sender);
        manager
            .handle_result(receiver.await, &mut processes, || {})
            .unwrap();
        let receiver = manager
            .dispatch(message())
            .expect("retry after closed channel");
        manager
            .handle_result(receiver.await, &mut processes, || {})
            .unwrap();
    }

    fn update_message(data: serde_json::Value) -> WsMessage {
        WsMessage {
            cmd: crate::network::protocol::WsCmd::Update,
            timestamp: 1,
            data: Some(data),
        }
    }

    #[test]
    fn parses_and_normalizes_valid_update_data() {
        let info = parse_update_info(update_message(json!({
            "version": "0.2.0",
            "target": "aarch64-apple-darwin",
            "url": "https://example.com/rsd",
            "sha256": "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"
        })))
        .expect("valid update data should parse");

        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.url, "https://example.com/rsd");
        assert_eq!(
            info.sha256,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn rejects_missing_update_data() {
        let msg = WsMessage {
            cmd: crate::network::protocol::WsCmd::Update,
            timestamp: 1,
            data: None,
        };
        assert!(parse_update_info(msg).is_err());
    }

    #[test]
    fn rejects_non_http_url() {
        let error = parse_update_info(update_message(json!({
            "version": "0.2.0",
            "target": "aarch64-apple-darwin",
            "url": "file:///tmp/rsd",
            "sha256": "0".repeat(SHA256_HEX_LEN)
        })))
        .expect_err("file URL should be rejected");

        assert!(error.to_string().contains("http"));
    }

    #[test]
    fn rejects_invalid_sha256() {
        for sha256 in ["0".repeat(SHA256_HEX_LEN - 1), "g".repeat(SHA256_HEX_LEN)] {
            let error = parse_update_info(update_message(json!({
                "version": "0.2.0",
                "target": "aarch64-apple-darwin",
                "url": "https://example.com/rsd",
                "sha256": sha256
            })))
            .expect_err("invalid SHA-256 should be rejected");

            assert!(error.to_string().contains("SHA-256"));
        }
    }

    #[test]
    fn rejects_empty_version() {
        let error = parse_update_info(update_message(json!({
            "version": " ",
            "target": "aarch64-apple-darwin",
            "url": "https://example.com/rsd",
            "sha256": "0".repeat(SHA256_HEX_LEN)
        })))
        .expect_err("empty version should be rejected");

        assert!(error.to_string().contains("version"));
    }

    #[test]
    fn rejects_empty_target() {
        let error = parse_update_info(update_message(json!({
            "version": "0.2.0",
            "target": " ",
            "url": "https://example.com/rsd",
            "sha256": "0".repeat(SHA256_HEX_LEN)
        })))
        .expect_err("empty target should be rejected");

        assert!(error.to_string().contains("target"));
    }
}
