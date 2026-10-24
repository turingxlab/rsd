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

use crate::args::Args;
use crate::config::RsdConfig;
use crate::network::protocol::{AppConfig, Application, Launcher, WsMessage};
use crate::network::websocket::{WebSocketClient, WsCmd, start_heartbeat};
use crate::process::{APP_CHECK_INTERVAL, ProcessManager};
use anyhow::Context;
use tracing_subscriber::fmt::time::LocalTime;

/// 启动 rsd 主程序
pub async fn run() -> anyhow::Result<()> {
    let (_, rsd_config) = bootstrap()?;

    // 启动早期计算设备信息（declare 上报用）；所有标识来源缺失时直接启动失败
    let device_info = device::device_info()?;

    // 输出启动信息
    banner::print_banner();
    tracing::info!("Application running.");

    // 初始化 WebSocket 连接（后台自动重连），得到可复用的发送/接收出口
    let ws_client = WebSocketClient::new(&rsd_config, &device_info);
    let handle = ws_client.connect();
    let mut process_manager = ProcessManager::default();

    // 启动心跳：复用同一条连接，定时发送 ping
    if rsd_config.websocket.heartbeat_enabled {
        start_heartbeat(
            handle.tx.clone(),
            rsd_config.websocket.heartbeat_interval_sec,
        );
    }

    // 监听服务端下发消息（如 pong 回应、Apps 配置），并定期检查 active 进程
    let mut rx = handle.rx;
    let mut check_interval = tokio::time::interval(APP_CHECK_INTERVAL);
    check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let shutdown_signal = tokio::signal::ctrl_c();
    tokio::pin!(shutdown_signal);

    let signal_result = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(msg) => match &msg.cmd {
                    WsCmd::Pong => tracing::debug!("Received pong"),
                    WsCmd::Config => {
                        if let Ok(text) = serde_json::to_string(&msg.data) {
                            tracing::debug!("Received config: {}", text);
                        }
                    }
                    WsCmd::Apps => match parse_active_app(msg) {
                        Ok(selected) => {
                            if let Err(error) = process_manager.apply_active_app(selected) {
                                tracing::error!("Failed to apply app config: {error:#}");
                            }
                        }
                        Err(error) => {
                            tracing::error!("Failed to parse app config: {error:#}");
                        }
                    },
                    _ => {
                        if let Ok(text) = serde_json::to_string(&msg) {
                            tracing::debug!("Unknown msg: {}", text);
                        }
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("Message listener lagged and skipped {skipped} messages");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break Ok(()),
            },
            _ = check_interval.tick() => {
                if let Err(error) = process_manager.check_and_restart() {
                    tracing::error!("Failed to check application: {error:#}");
                }
            },
            result = &mut shutdown_signal => {
                break result.context("Failed to listen for Ctrl-C");
            },
        }
    };

    if let Err(error) = process_manager.shutdown() {
        tracing::warn!(%error, "Failed to stop applications");
    }

    signal_result?;
    Ok(())
}

fn bootstrap() -> anyhow::Result<(Args, RsdConfig)> {
    // 尽早解析启动参数：--help / --version / 非法参数由 clap 直接输出并退出（0 或 2），
    // 避免帮助与错误信息被启动横幅和日志输出干扰。
    let args = args::Args::parse_args();

    // 加载配置文件（-c 显式指定优先，否则回退到可执行文件同目录下的 rsd.toml）
    let rsd_config = RsdConfig::load_from_file(args.config()?)?;

    // 初始化日志工具
    let timer = LocalTime::new(
        time::format_description::parse_borrowed::<3>(
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]",
        )
        .expect("Invalid time format"),
    );
    // 过滤器优先级：RUST_LOG（有效时）> rsd.toml [logging].level > 内置默认 info
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&rsd_config.logging.level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .init();

    Ok((args, rsd_config))
}

fn parse_active_app(msg: WsMessage) -> anyhow::Result<Option<(Application, Launcher)>> {
    let app_config: AppConfig = serde_json::from_value(msg.data.context("Apps data is missing")?)
        .context("Failed to parse app config")?;

    match toml::to_string_pretty(&app_config) {
        Ok(text) => tracing::debug!("Received app config:\n{text}"),
        Err(error) => tracing::warn!("Failed to format app config as TOML: {error:#}"),
    }

    let Some(application) = app_config.applications.iter().find(|app| app.active) else {
        return Ok(None);
    };
    let launcher = app_config
        .launchers
        .iter()
        .find(|launcher| launcher.launcher_id == application.launcher_id)
        .with_context(|| {
            format!(
                "Launcher {} not found for application {}",
                application.launcher_id, application.app_id
            )
        })?;

    Ok(Some((application.clone(), launcher.clone())))
}
