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

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// tls 省略时的默认值
const DEFAULT_TLS: bool = true;
/// tls 为 true 且 port 省略时的默认端口
const DEFAULT_TLS_PORT: u16 = 443;
/// tls 为 false 且 port 省略时的默认端口
const DEFAULT_PLAIN_PORT: u16 = 80;

/// 日志等级缺省（RUST_LOG 与 [logging].level 均未指定）时的默认值
const DEFAULT_LOG_LEVEL: &str = "info";

/// websocket.path 省略时的默认值
const DEFAULT_WS_PATH: &str = "/device/rsd/cmd";
/// websocket.reconnect_interval_sec 省略时的默认值
const DEFAULT_WS_RECONNECT_INTERVAL_SEC: u64 = 30;
/// websocket.heartbeat_enabled 省略时的默认值
const DEFAULT_WS_HEARTBEAT_ENABLED: bool = true;
/// websocket.heartbeat_interval_sec 省略时的默认值
const DEFAULT_WS_HEARTBEAT_INTERVAL_SEC: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ServerConfigRaw")]
pub struct ServerConfig {
    pub addr: String,
    pub port: u16,
    pub tls: bool,
}

/// 反序列化中间结构：`port` / `tls` 可选，`port` 的默认值需要跟随 `tls`，
/// 单字段 `#[serde(default)]` 无法表达这种联动，因此用中间结构转换。
#[derive(Deserialize)]
struct ServerConfigRaw {
    addr: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    tls: Option<bool>,
}

impl From<ServerConfigRaw> for ServerConfig {
    fn from(raw: ServerConfigRaw) -> Self {
        let tls = raw.tls.unwrap_or(DEFAULT_TLS);
        let port = raw.port.unwrap_or(if tls {
            DEFAULT_TLS_PORT
        } else {
            DEFAULT_PLAIN_PORT
        });

        Self {
            addr: raw.addr,
            port,
            tls,
        }
    }
}

/// 结构体级 `#[serde(default)]`：表内字段可部分省略，缺失字段取 `Default`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebsocketConfig {
    pub path: String,
    pub reconnect_interval_sec: u64,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_sec: u64,
}

impl Default for WebsocketConfig {
    fn default() -> Self {
        Self {
            path: DEFAULT_WS_PATH.to_string(),
            reconnect_interval_sec: DEFAULT_WS_RECONNECT_INTERVAL_SEC,
            heartbeat_enabled: DEFAULT_WS_HEARTBEAT_ENABLED,
            heartbeat_interval_sec: DEFAULT_WS_HEARTBEAT_INTERVAL_SEC,
        }
    }
}

/// 结构体级 `#[serde(default)]`：表内字段可部分省略，缺失字段取 `Default`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// 日志过滤指令：单个等级名（debug/info/warn/error/trace），
    /// 或完整 EnvFilter 指令（如 "rsd=debug,tokio=info"）
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    /// 整个 `[websocket]` 表省略时取默认配置
    #[serde(default)]
    pub websocket: WebsocketConfig,
    /// 整个 `[logging]` 表省略时取默认配置
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl AppConfig {
    /// 从指定的 TOML 文件路径加载配置
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path_ref = path.as_ref();

        let content =
            fs::read_to_string(path_ref).with_context(|| format!("{}", path_ref.display()))?;

        let config: AppConfig =
            toml::from_str(&content).with_context(|| format!("{}", path_ref.display()))?;

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> anyhow::Result<AppConfig> {
        Ok(toml::from_str(content)?)
    }

    #[test]
    fn parses_full_config() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"
            port = 8080
            tls = false

            [websocket]
            path = "/custom/path"
            reconnect_interval_sec = 30
            heartbeat_enabled = false
            heartbeat_interval_sec = 30

            [logging]
            level = "debug"
            "#,
        )
        .expect("full config should parse");

        assert_eq!(
            config.server,
            ServerConfig {
                addr: "echo.websocket.org".to_string(),
                port: 8080,
                tls: false,
            }
        );
        assert_eq!(
            config.websocket,
            WebsocketConfig {
                path: "/custom/path".to_string(),
                reconnect_interval_sec: 30,
                heartbeat_enabled: false,
                heartbeat_interval_sec: 30,
            }
        );
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn defaults_tls_and_port_when_omitted() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"
            "#,
        )
        .expect("server without port/tls should parse");

        assert!(config.server.tls);
        assert_eq!(config.server.port, DEFAULT_TLS_PORT);
    }

    #[test]
    fn defaults_port_to_plain_when_tls_disabled() {
        let config = parse(
            r#"
            [server]
            addr = "127.0.0.1"
            tls = false
            "#,
        )
        .expect("server without port should parse");

        assert!(!config.server.tls);
        assert_eq!(config.server.port, DEFAULT_PLAIN_PORT);
    }

    #[test]
    fn keeps_explicit_port() {
        let config = parse(
            r#"
            [server]
            addr = "127.0.0.1"
            port = 8080
            tls = false
            "#,
        )
        .expect("server with explicit port should parse");

        assert_eq!(config.server.port, 8080);
        assert!(!config.server.tls);
    }

    #[test]
    fn defaults_websocket_when_table_omitted() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"
            "#,
        )
        .expect("missing [websocket] table should parse");

        assert_eq!(config.websocket, WebsocketConfig::default());
        assert_eq!(config.websocket.path, DEFAULT_WS_PATH);
        assert_eq!(
            config.websocket.reconnect_interval_sec,
            DEFAULT_WS_RECONNECT_INTERVAL_SEC
        );
        assert_eq!(
            config.websocket.heartbeat_enabled,
            DEFAULT_WS_HEARTBEAT_ENABLED
        );
        assert_eq!(
            config.websocket.heartbeat_interval_sec,
            DEFAULT_WS_HEARTBEAT_INTERVAL_SEC
        );
    }

    #[test]
    fn fills_missing_websocket_fields() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"

            [websocket]
            path = "/custom/path"
            "#,
        )
        .expect("partially specified [websocket] should parse");

        assert_eq!(config.websocket.path, "/custom/path");
        assert_eq!(
            config.websocket.reconnect_interval_sec,
            DEFAULT_WS_RECONNECT_INTERVAL_SEC
        );
        assert!(config.websocket.heartbeat_enabled);
        assert_eq!(
            config.websocket.heartbeat_interval_sec,
            DEFAULT_WS_HEARTBEAT_INTERVAL_SEC
        );
    }

    #[test]
    fn defaults_logging_when_omitted() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"
            "#,
        )
        .expect("missing [logging] table should parse");

        assert_eq!(config.logging.level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn parses_logging_level() {
        let config = parse(
            r#"
            [server]
            addr = "echo.websocket.org"

            [logging]
            level = "warn"
            "#,
        )
        .expect("[logging] level should parse");

        assert_eq!(config.logging.level, "warn");
    }

    #[test]
    fn rejects_missing_addr() {
        assert!(parse("[server]\nport = 443\n").is_err());
        assert!(parse("[websocket]\npath = \"/x\"\n").is_err());
    }
}
