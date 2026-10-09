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
mod device;
mod network;
mod process;
mod utils;

use crate::args::Args;
use crate::config::AppConfig;
use crate::network::protocol::{Settings, WsMessage};
use crate::network::websocket::{WebSocketClient, WsCmd, start_heartbeat};
use crate::process::ProcessManager;
use anyhow::Context;
use std::fs;
use tracing_subscriber::fmt::time::LocalTime;

/// 启动 rsd 主程序
pub async fn run() -> anyhow::Result<()> {
    let (_, app_config) = bootstrap()?;

    // 启动早期计算设备信息（declare 上报用）；所有标识来源缺失时直接启动失败
    let device_info = device::device_info()?;

    // 输出启动信息
    banner::print_banner();
    tracing::info!("Application running.");

    // 初始化 WebSocket 连接（后台自动重连），得到可复用的发送/接收出口
    let ws_client = WebSocketClient::new(&app_config, &device_info);
    let handle = ws_client.connect();
    let process_manager = ProcessManager::default();

    // 启动心跳：复用同一条连接，定时发送 ping
    if app_config.websocket.heartbeat_enabled {
        start_heartbeat(
            handle.tx.clone(),
            app_config.websocket.heartbeat_interval_sec,
        );
    }

    // 监听服务端下发消息（如 pong 回应、config 配置）
    let mut rx = handle.rx.resubscribe();
    let process_manager_for_messages = process_manager.clone();
    let message_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            match &msg.cmd {
                WsCmd::Pong => tracing::debug!("Received pong"),
                WsCmd::Config => {
                    if let Ok(text) = serde_json::to_string(&msg.data) {
                        tracing::debug!("Received config: {}", text);
                    }
                }
                WsCmd::Settings => {
                    if let Err(error) = apply_settings(msg, &process_manager_for_messages) {
                        tracing::error!("Failed to apply settings: {error:#}");
                    }
                }
                _ => {
                    if let Ok(text) = serde_json::to_string(&msg) {
                        tracing::debug!("Unknown msg: {}", text);
                    }
                }
            }
        }
    });

    let signal_result = tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for Ctrl-C");

    message_task.abort();
    let _ = message_task.await;
    if let Err(error) = process_manager.stop_all() {
        tracing::warn!(%error, "Failed to stop applications");
    }

    signal_result?;
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
        .expect("Invalid time format"),
    );
    // 过滤器优先级：RUST_LOG（有效时）> config.toml [logging].level > 内置默认 info
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&app_config.logging.level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .init();

    Ok((args, app_config))
}

fn apply_settings(msg: WsMessage, process_manager: &ProcessManager) -> anyhow::Result<()> {
    let settings: Settings = serde_json::from_value(msg.data.context("Settings data is missing")?)
        .context("Failed to parse settings")?;

    let text = toml::to_string_pretty(&settings).context("Failed to serialize settings")?;
    tracing::debug!("Received settings:\n{}", text);
    let config_path = utils::exe_dir()
        .context("Failed to locate executable directory")?
        .join("settings.toml");
    fs::write(config_path, text).context("Failed to save settings")?;

    let application = settings.applications.iter().find(|app| app.active);
    process_manager.stop_except(application.map(|app| app.app_id.as_str()))?;
    let launcher = application
        .map(|app| {
            settings
                .launchers
                .iter()
                .find(|launcher| launcher.launcher_id == app.launcher_id)
                .with_context(|| {
                    format!(
                        "Launcher {} not found for application {}",
                        app.launcher_id, app.app_id
                    )
                })
        })
        .transpose()?;
    if let (Some(application), Some(launcher)) = (application, launcher) {
        process_manager.start(application, launcher)?;
    }
    Ok(())
}
