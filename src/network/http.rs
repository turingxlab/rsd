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

use crate::config::RsdConfig;
use anyhow::Context;
use reqwest::multipart::{Form, Part};
use std::{
    path::Path,
    sync::{LazyLock, OnceLock},
    time::Duration,
};
use url::Url;

/// 全局唯一的 HTTP 客户端。LazyLock 在第一次使用时创建，之后复用同一实例。
/// reqwest 客户端内部带连接池，复用它可以避免重复建立 TCP/TLS 连接。
pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        // 仅限制建立连接阶段的最长等待时间。
        .connect_timeout(Duration::from_secs(10))
        // 24 小时的总请求上限，避免大文件下载被短超时中断。
        .timeout(Duration::from_secs(24 * 60 * 60))
        // 连接建立后连续 60 秒没有数据则认为连接异常。
        .read_timeout(Duration::from_secs(60))
        // 每个 host 最多保留 8 条空闲连接供后续请求复用。
        .pool_max_idle_per_host(8)
        // 空闲连接超过 90 秒后从连接池移除。
        .pool_idle_timeout(Duration::from_secs(90))
        // 便于服务端日志识别客户端。
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        // 客户端是固定配置，构建失败属于不可恢复错误。
        .expect("Failed to build HTTP client")
});

static API: OnceLock<Api> = OnceLock::new();

/// 服务端 API，初始化后复用同一服务地址。
pub struct Api {
    base_url: Url,
}

/// 待上传的内存文件，上传时直接接管字节数据。
pub struct UploadFile {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

impl Api {
    pub fn init(config: &RsdConfig) -> anyhow::Result<()> {
        let scheme = if config.server.tls { "https" } else { "http" };
        let mut base_url = Url::parse(&format!("{scheme}://localhost"))
            .context("Failed to create API base URL")?;
        base_url
            .set_host(Some(&config.server.addr))
            .context("Failed to set API server address")?;
        base_url
            .set_port(Some(config.server.port))
            .map_err(|_| anyhow::anyhow!("Failed to set API server port"))?;
        base_url.set_path(&format!("{}/", config.http.path.trim_end_matches('/')));
        API.set(Self { base_url })
            .map_err(|_| anyhow::anyhow!("API is already initialized"))
    }

    pub async fn upload_screenshots(files: Vec<UploadFile>) -> anyhow::Result<String> {
        let api = API.get().context("API is not initialized")?;
        api.upload_screenshot_files(files).await
    }

    pub async fn upload_logs(archive_path: &Path) -> anyhow::Result<String> {
        let api = API.get().context("API is not initialized")?;
        api.upload_log_archive(archive_path).await
    }

    async fn upload_log_archive(&self, archive_path: &Path) -> anyhow::Result<String> {
        let part = Part::file(archive_path)
            .await
            .context("Failed to open log archive")?
            .mime_str("application/gzip")
            .context("Failed to set log archive content type")?;
        self.upload("log", Form::new().part("files", part)).await
    }

    async fn upload_screenshot_files(&self, files: Vec<UploadFile>) -> anyhow::Result<String> {
        anyhow::ensure!(!files.is_empty(), "Upload files must not be empty");
        let mut form = Form::new();
        for file in files {
            let part = Part::bytes(file.data)
                .file_name(file.filename)
                .mime_str(&file.content_type)
                .context("Failed to set upload file content type")?;
            form = form.part("files", part);
        }
        self.upload("screenshot", form).await
    }

    async fn upload(&self, path: &str, form: Form) -> anyhow::Result<String> {
        let url = self
            .base_url
            .join(path)
            .context("Failed to build upload URL")?;
        HTTP_CLIENT
            .post(url)
            .timeout(Duration::from_secs(60))
            .multipart(form)
            .send()
            .await
            .context("Failed to upload files")?
            .error_for_status()
            .context("Upload returned an unsuccessful HTTP status")?
            .text()
            .await
            .context("Failed to read upload response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn file(filename: &str, data: &[u8]) -> UploadFile {
        UploadFile {
            filename: filename.to_string(),
            content_type: "image/png".to_string(),
            data: data.to_vec(),
        }
    }

    // 直接创建实例，避免测试之间共享不可重复初始化的全局状态。
    async fn mock_server(status: &str) -> (Api, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = Api {
            base_url: Url::parse(&format!(
                "http://{}/device/rsd/",
                listener.local_addr().unwrap()
            ))
            .unwrap(),
        };
        let response =
            format!("HTTP/1.1 {status}\r\nContent-Length: 7\r\nConnection: close\r\n\r\nsuccess");
        let task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "Request ended before its body was received");
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .expect("Multipart should have a known content length");
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                stream.write_all(response.as_bytes()).await.unwrap();
                String::from_utf8(request).unwrap()
            })
            .await
            .expect("Mock server timed out")
        });
        (api, task)
    }

    #[tokio::test]
    async fn uploads_multiple_screenshots() {
        let (api, server) = mock_server("200 OK").await;
        let response = api
            .upload_screenshot_files(vec![
                file("screenshot-1.png", b"first-image"),
                file("screenshot-2.png", b"second-image"),
            ])
            .await
            .unwrap();
        assert_eq!(response, "success");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /device/rsd/screenshot HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: multipart/form-data; boundary=")
        );
        assert_eq!(request.matches("name=\"files\"").count(), 2);
        assert_eq!(request.matches("Content-Type: image/png").count(), 2);
        for (filename, data) in [
            ("screenshot-1.png", "first-image"),
            ("screenshot-2.png", "second-image"),
        ] {
            assert!(request.contains(&format!(
                "filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n{data}\r\n"
            )));
        }
    }

    #[tokio::test]
    async fn uploads_log_archive_to_custom_prefix() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("rsd-logs-123.tar.gz");
        tokio::fs::write(&path, b"archive-content").await.unwrap();
        let (mut api, server) = mock_server("200 OK").await;
        api.base_url.set_path("/custom/api/");
        assert_eq!(api.upload_log_archive(&path).await.unwrap(), "success");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /custom/api/log HTTP/1.1\r\n"));
        assert!(request.contains("name=\"files\"; filename=\"rsd-logs-123.tar.gz\""));
        assert!(request.contains("Content-Type: application/gzip\r\n\r\narchive-content"));
    }

    #[tokio::test]
    async fn rejects_http_errors() {
        let (api, server) = mock_server("500 Internal Server Error").await;
        assert!(
            api.upload_screenshot_files(vec![file("screenshot.png", b"image")])
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_empty_files_and_connection_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = Api {
            base_url: Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap(),
        };
        let error = api.upload_screenshot_files(Vec::new()).await.unwrap_err();
        assert_eq!(error.to_string(), "Upload files must not be empty");
        drop(listener);
        assert!(
            api.upload_screenshot_files(vec![file("screenshot.png", b"image")])
                .await
                .is_err()
        );
    }
}
