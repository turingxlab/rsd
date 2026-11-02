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
mod logging;
mod network;
mod process;
mod runtime;
mod screen;
mod update;

use crate::args::Args;
use crate::config::RsdConfig;
use crate::network::http::Api;
use crate::network::websocket::{WebSocketClient, start_heartbeat};
use crate::runtime::Runtime;
use anyhow::Context;

/// 启动 rsd 主程序
pub async fn run() -> anyhow::Result<()> {
    let (_, rsd_config, mut logging) = bootstrap()?;
    Api::init(&rsd_config)?;

    // 启动早期计算设备信息（declare 上报用）；所有标识来源缺失时直接启动失败
    let device_info = device::device_info()?;

    // 输出启动信息
    banner::print_banner(&mut logging).context("Failed to write startup banner")?;
    tracing::info!("Started rsd (PID {})", std::process::id());

    // 初始化 WebSocket 连接（后台自动重连），得到可复用的发送/接收出口
    let ws_client = WebSocketClient::new(&rsd_config, &device_info);
    let handle = ws_client.connect();

    // 启动心跳：复用同一条连接，定时发送 ping
    if rsd_config.websocket.heartbeat_enabled {
        start_heartbeat(
            handle.tx.clone(),
            rsd_config.websocket.heartbeat_interval_sec,
        );
    }

    let mut runtime = Runtime::new(rsd_config.logging);
    let result = runtime.run_loop(handle.rx, &mut logging).await;
    runtime.shutdown().await;
    result
}

fn bootstrap() -> anyhow::Result<(Args, RsdConfig, logging::Logging)> {
    // 尽早解析启动参数：--help / --version / 非法参数由 clap 直接输出并退出（0 或 2），
    // 避免帮助与错误信息被启动横幅和日志输出干扰。
    let args = args::Args::parse_args();

    // 加载配置文件（-c 显式指定优先，否则回退到可执行文件同目录下的 rsd.toml）
    let rsd_config = RsdConfig::load_from_file(args.config()?)?;

    let logging = logging::init(&rsd_config.logging)?;

    Ok((args, rsd_config, logging))
}
