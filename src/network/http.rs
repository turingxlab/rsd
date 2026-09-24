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

use std::{sync::LazyLock, time::Duration};

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
        .expect("failed to build HTTP client")
});
