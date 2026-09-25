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

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// WebSocket 消息命令。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsCmd {
    Declare,
    Ping,
    Pong,
    Config,
}

/// 一条 ws 消息，统一结构：cmd + timestamp + 可选 data。
///
/// `cmd` 标识消息类型：
/// - `WsCmd::Declare`：客户端声明（data 为设备信息）
/// - `WsCmd::Ping`：客户端心跳（无 data）
/// - `WsCmd::Pong`：服务端回应心跳
/// - `WsCmd::Config`：服务端下发配置（data 为配置内容）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WsMessage {
    /// 消息类型
    pub cmd: WsCmd,
    /// Unix 毫秒时间戳
    pub timestamp: u64,
    /// 可选负载：心跳不带，业务数据 / 服务端下发携带
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl WsMessage {
    /// 构造一条无负载消息（如心跳 ping）
    pub fn new(cmd: WsCmd) -> Self {
        Self {
            cmd,
            timestamp: now_ms(),
            data: None,
        }
    }

    /// 构造一条带负载消息（业务数据 / 服务端下发）
    pub fn with_data(cmd: WsCmd, data: serde_json::Value) -> Self {
        Self {
            cmd,
            timestamp: now_ms(),
            data: Some(data),
        }
    }
}

/// `WsCmd::Declare`消息类型的`data`结构，设备信息
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeviceInfo {
    /// 设备唯一标识：对硬件/系统标识做 SHA-256（大写 hex）并截取前 16 位得到的定长 ID
    pub device_id: String,
    /// 设备 IP 地址
    pub ip: String,
    /// 设备版本号
    pub version: String,
}

fn now_ms() -> u64 {
    OffsetDateTime::now_utc().unix_timestamp() as u64 * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&WsCmd::Declare).unwrap(),
            r#""declare""#
        );
        assert_eq!(serde_json::to_string(&WsCmd::Ping).unwrap(), r#""ping""#);
        assert_eq!(serde_json::to_string(&WsCmd::Pong).unwrap(), r#""pong""#);
        assert_eq!(
            serde_json::to_string(&WsCmd::Config).unwrap(),
            r#""config""#
        );
    }

    #[test]
    fn unknown_cmd_is_rejected() {
        assert!(serde_json::from_str::<WsCmd>(r#""future_cmd""#).is_err());
    }

    #[test]
    fn ws_message_keeps_existing_wire_format() {
        let msg = WsMessage::new(WsCmd::Ping);
        let json = serde_json::to_value(&msg).unwrap();

        assert_eq!(json["cmd"], "ping");
        assert!(json["timestamp"].is_u64());
        assert!(json.get("data").is_none());
    }

    #[test]
    fn ws_message_supports_optional_data() {
        let data = serde_json::json!({ "key": "value" });
        let msg = WsMessage::with_data(WsCmd::Config, data.clone());
        let json = serde_json::to_value(&msg).unwrap();

        assert_eq!(json["cmd"], "config");
        assert_eq!(json["data"], data);

        let decoded = serde_json::from_value::<WsMessage>(json).unwrap();
        assert_eq!(decoded.cmd, WsCmd::Config);
        assert_eq!(decoded.data, Some(data));
    }
}
