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

use crate::network::http::HTTP_CLIENT;
use reqwest::{StatusCode, header};
use std::io;
use std::path::Path;
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// 下载过程中可能出现的错误类型，便于调用方按类别处理。
#[derive(Debug, Error)]
pub enum DownloadError {
    /// HTTP 请求错误，例如连接失败、网络中断。
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// 本地文件操作错误，例如无法创建或写入文件。
    #[error("download file operation failed: {0}")]
    Io(#[from] io::Error),

    /// 服务器返回了非预期的 HTTP 状态码。
    #[error("unexpected HTTP status: {0}")]
    UnexpectedStatus(StatusCode),
}

/// 将 `url` 下载到 `destination`。
///
/// 如果目标文件已存在，则把它视为上次中断留下的部分文件，尝试从当前长度处
/// 继续下载。目标文件本身就是续传载体，不额外创建 `.part` 文件。
pub async fn download(url: &str, destination: impl AsRef<Path>) -> Result<(), DownloadError> {
    // `impl AsRef<Path>` 允许传入 &str、String、Path 或 PathBuf；as_ref() 只借用路径。
    let destination = destination.as_ref();
    // 确保目标的父目录存在。
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).await?;
        }
    }

    // 查询本地目标文件大小：普通文件用于计算 Range，不存在则按 0 字节处理。
    let existing_len = match fs::metadata(destination).await {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "destination is not a file").into(),
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };

    // append=true 表示追加续传；append=false 表示覆盖并从头下载。
    let (mut response, append) = if existing_len > 0 {
        // 本地已有内容时，请求服务端返回从 existing_len 开始的剩余部分。
        let ranged = HTTP_CLIENT
            .get(url)
            .header(header::RANGE, format!("bytes={existing_len}-"))
            .send()
            .await?;
        match ranged.status() {
            // 只有 206 且 Content-Range 起点匹配时，追加才是安全的。
            StatusCode::PARTIAL_CONTENT if valid_content_range(&ranged, existing_len) => {
                (ranged, true)
            }
            // 206 但范围非法，重新请求完整文件，避免错误追加。
            StatusCode::PARTIAL_CONTENT => (full_response(url).await?, false),
            // 服务端忽略 Range 并返回完整文件，改为覆盖写入。
            StatusCode::OK => (ranged, false),
            // 其他状态（如 404、416）不修改本地文件，直接返回错误。
            status => return Err(DownloadError::UnexpectedStatus(status)),
        }
    } else {
        // 没有可续传内容，直接请求完整文件。
        (full_response(url).await?, false)
    };

    // 确认 HTTP 状态成功后才打开目标文件，避免请求失败时破坏原文件。
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(destination)
        .await?;
    // 每次只读取一小块响应体，适合下载大文件，不会一次性占满内存。
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    // 刷新文件缓冲区，确保写入请求已经提交。
    file.flush().await?;
    Ok(())
}

async fn full_response(url: &str) -> Result<reqwest::Response, DownloadError> {
    // 发起不带 Range 的完整下载请求，只接受 200 OK。
    let response = HTTP_CLIENT.get(url).send().await?;
    if response.status() == StatusCode::OK {
        Ok(response)
    } else {
        Err(DownloadError::UnexpectedStatus(response.status()))
    }
}

fn valid_content_range(response: &reqwest::Response, expected_start: u64) -> bool {
    // Content-Range 格式为 `bytes 起始位置-结束位置/完整文件大小`，例如：
    // `bytes 1000-1999/5000`。这里只需确认起始位置和结束位置有效。

    // 先从响应头中取出 Content-Range。
    // 服务器没有返回这个头时，无法确认响应体从哪个字节开始，只能认为不能续传。
    let Some(value) = response.headers().get(header::CONTENT_RANGE) else {
        return false;
    };

    // HTTP 响应头底层是字节序列，这里将它转换成字符串。
    // 如果响应头不是合法的 UTF-8，也无法继续解析。
    let Ok(value) = value.to_str() else {
        return false;
    };

    // 只接受以 `bytes ` 开头的字节范围格式。
    // 例如 `items 1000-1999/5000` 就不是合法的字节范围响应。
    let Some(range) = value.strip_prefix("bytes ") else {
        return false;
    };

    // 按第一个 `-` 拆出起始位置和剩余部分：
    // `1000-1999/5000` -> `start = "1000"`、`end_and_total = "1999/5000"`。
    let Some((start, end_and_total)) = range.split_once('-') else {
        return false;
    };

    // 再按 `/` 拆出结束位置和完整文件大小。
    // 这里用 `_` 接收并忽略完整文件大小，因为当前只校验范围起点和终点。
    let Some((end, _)) = end_and_total.split_once('/') else {
        return false;
    };

    // 将起始位置和结束位置从字符串转换为 u64 字节偏移量。
    match (start.parse::<u64>(), end.parse::<u64>()) {
        // 续传安全需要同时满足：
        // 1. 服务端返回的起点等于本地文件已有长度；
        // 2. 结束位置不能小于起始位置。
        (Ok(start), Ok(end)) => start == expected_start && end >= start,
        // 任意一个位置不是合法数字，都不能安全追加。
        _ => false,
    }
}
