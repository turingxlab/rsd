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

use crate::config::AppConfig;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

pub use crate::network::protocol::{DeviceInfo, WsCmd, WsMessage};

/// 通用 WebSocket 连接管理器：负责建立连接、自动重连，
/// 并通过发送/接收通道对外复用同一条连接。
pub struct WebSocketClient {
    url: String,
    reconnect_interval: Duration,
    /// 连接建立后随 declare 上报的设备信息
    device_info: DeviceInfo,
}

impl WebSocketClient {
    pub fn new(app_config: &AppConfig, device_info: &DeviceInfo) -> Self {
        Self {
            url: Self::url(app_config),
            reconnect_interval: Duration::from_secs(app_config.websocket.reconnect_interval_sec),
            device_info: device_info.clone(),
        }
    }

    /// 建立连接并返回可复用的发送 / 接收出口。
    /// 连接在后台维护，断开会自动重连，调用方无需感知。
    pub fn connect(&self) -> ConnectionHandle {
        let (tx, mut rx) = mpsc::channel::<WsMessage>(128);
        let (broadcast_tx, broadcast_rx) = broadcast::channel::<WsMessage>(128);

        let url = self.url.clone();
        let reconnect_interval = self.reconnect_interval;
        let device_info = self.device_info.clone();

        tokio::spawn(async move {
            loop {
                let outcome =
                    Self::run_connection(&url, &mut rx, &broadcast_tx, &device_info).await;
                match outcome {
                    // 所有发送方已关闭，说明业务已结束，不再重连
                    ConnectionOutcome::Stop => {
                        tracing::info!("WebSocket client stopped (all senders closed).");
                        return;
                    }
                    // 连接断开或出错，等待一段时间后重连
                    ConnectionOutcome::Reconnect => {
                        tracing::info!("Reconnecting in {:?}...", reconnect_interval);
                        tokio::time::sleep(reconnect_interval).await;
                    }
                }
            }
        });

        ConnectionHandle {
            tx,
            rx: broadcast_rx,
        }
    }

    fn url(app_config: &AppConfig) -> String {
        let scheme = match app_config.server.tls {
            true => "wss",
            false => "ws",
        };

        let url = Url::parse(
            format!(
                "{}://{}:{}{}",
                scheme, app_config.server.addr, app_config.server.port, app_config.websocket.path,
            )
            .as_str(),
        )
        .unwrap();

        url.to_string()
    }

    /// 连接 → 拆流 → 声明设备信息 → 双泵收发，直到断连或所有发送方关闭。
    async fn run_connection(
        url: &str,
        rx: &mut mpsc::Receiver<WsMessage>,
        broadcast_tx: &broadcast::Sender<WsMessage>,
        device_info: &DeviceInfo,
    ) -> ConnectionOutcome {
        Self::connect_once(url, rx, broadcast_tx, device_info)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Connection failed: {}", e);
                ConnectionOutcome::Reconnect
            })
    }

    async fn connect_once(
        url: &str,
        rx: &mut mpsc::Receiver<WsMessage>,
        broadcast_tx: &broadcast::Sender<WsMessage>,
        device_info: &DeviceInfo,
    ) -> anyhow::Result<ConnectionOutcome> {
        tracing::info!("Connecting to {}", url);

        let request = url.into_client_request()?;
        let (ws_stream, response) = connect_async(request).await?;
        tracing::info!("Connected. HTTP status: {}", response.status());

        let (mut write, mut read) = ws_stream.split();

        // 连接建立后立即声明设备信息；重连成功时同样发送
        let declare = WsMessage::with_data(WsCmd::Declare, serde_json::to_value(device_info)?);
        let text = serde_json::to_string(&declare)?;
        if let Err(e) = write.send(Message::Text(text.into())).await {
            tracing::warn!("Failed to send declare: {}", e);
        }

        // 发送泵：从 mpsc 队列取消息，序列化成 JSON 写入连接
        let send_pump = async {
            while let Some(ws_msg) = rx.recv().await {
                let Ok(text) = serde_json::to_string(&ws_msg) else {
                    tracing::error!("Failed to serialize message: {:?}", ws_msg);
                    return SendEnd::Error;
                };
                tracing::debug!("Send: {}", text);
                if let Err(e) = write.send(Message::Text(text.into())).await {
                    tracing::warn!("Send error: {}", e);
                    // 连接可能已断开，触发重连
                    return SendEnd::Error;
                }
            }
            // 所有发送方已 drop（业务结束），停止连接
            SendEnd::Closed
        };

        // 接收泵：把收到的 JSON 反序列化成 WsMessage 广播给订阅者
        let recv_pump = async {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<WsMessage>(&text) {
                            Ok(ws_msg) => {
                                // 忽略发送失败的后果（如所有订阅者已 drop）
                                let _ = broadcast_tx.send(ws_msg);
                            }
                            Err(_) => tracing::warn!("Ignoring unparseable message: {}", text),
                        }
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!("Server closed connection.");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Receive error: {}", e);
                        break;
                    }
                }
            }
        };

        let outcome = tokio::select! {
            _ = recv_pump => ConnectionOutcome::Reconnect,
            end = send_pump => match end {
                SendEnd::Error => ConnectionOutcome::Reconnect,
                SendEnd::Closed => ConnectionOutcome::Stop,
            },
        };

        Ok(outcome)
    }
}

/// 连接管理器后台循环的结束方式
enum ConnectionOutcome {
    /// 连接断开或出错，等待后重连
    Reconnect,
    /// 所有发送方已关闭，停止连接
    Stop,
}

/// 发送泵的结束原因
enum SendEnd {
    /// 发送出错 / 序列化失败（连接问题，需重连）
    Error,
    /// mpsc 队列已关闭（所有发送方 drop，业务结束）
    Closed,
}

/// 一条对外可复用的连接句柄：
/// - `tx`：发送出口，多个功能可各自 clone 一份发消息
/// - `rx`：接收出口，多个订阅者可各自 resubscribe 收消息
pub struct ConnectionHandle {
    pub tx: mpsc::Sender<WsMessage>,
    pub rx: broadcast::Receiver<WsMessage>,
}

/// 独立的心跳功能：复用共享连接，定时发送 ping。
///
/// 只发不收；若连接已关闭（send 失败），记录日志后退出。
pub fn start_heartbeat(tx: mpsc::Sender<WsMessage>, interval_sec: u64) {
    tokio::spawn(async move {
        // 间隔器自动对齐系统时钟，跳过积压的过期 tick，保持最新节奏
        let mut interval = tokio::time::interval(Duration::from_secs(interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let msg = WsMessage::new(WsCmd::Ping);
            if tx.send(msg).await.is_err() {
                tracing::warn!("Heartbeat sender closed (connection gone).");
                break;
            }
        }
    });
}
