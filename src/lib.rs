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

mod args;
mod banner;
mod config;
mod network;

use crate::args::Args;
use crate::config::AppConfig;
use crate::network::websocket::{WebSocketClient, WsCmd, start_heartbeat};
use anyhow::Context;
use tracing_subscriber::fmt::time::LocalTime;

/// 启动 rsd 主程序
pub async fn run() -> anyhow::Result<()> {
    let (_, app_config) = bootstrap()?;

    // 初始化 WebSocket 连接（后台自动重连），得到可复用的发送/接收出口
    let ws_client = WebSocketClient::new(&app_config);
    let handle = ws_client.connect();

    // 启动心跳：复用同一条连接，定时发送 ping
    if app_config.websocket.heartbeat_enabled {
        start_heartbeat(
            handle.tx.clone(),
            app_config.websocket.heartbeat_interval_sec,
        );
    }

    // 监听服务端下发消息（如 pong 回应、config 配置）
    let mut rx = handle.rx.resubscribe();
    tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            match &msg.cmd {
                WsCmd::Pong => tracing::debug!("Received pong"),
                WsCmd::Config => tracing::debug!("Received config: {:?}", msg.data),
                _ => {
                    if let Ok(text) = serde_json::to_string(&msg) {
                        tracing::debug!("Unknown msg: {}", text);
                    }
                }
            }
        }
    });

    tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for Ctrl-C")?;

    Ok(())
}

fn bootstrap() -> anyhow::Result<(Args, AppConfig)> {
    // 尽早解析启动参数：--help / --version / 非法参数由 clap 直接输出并退出（0 或 2），
    // 避免帮助与错误信息被启动横幅和日志输出干扰。
    let args = args::Args::parse_args();

    // 加载配置文件（-c 显式指定优先，否则回退到可执行文件同目录下的 config.toml）
    let app_config = AppConfig::load_from_file(args.config()?)?;

    // 初始化日志工具
    let timer = LocalTime::new(
        time::format_description::parse_borrowed::<3>(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]",
        )
        .expect("invalid time format"),
    );
    // 过滤器优先级：RUST_LOG（有效时）> config.toml [logging].level > 内置默认 info
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&app_config.logging.level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .init();

    // 输出启动信息
    banner::print_banner();
    tracing::info!("Application running.");

    Ok((args, app_config))
}
