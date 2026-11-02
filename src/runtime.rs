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

use crate::config::LoggingConfig;
use crate::network::protocol::{AppConfig, Application, Launcher, WsCmd, WsMessage};
use crate::process::{APP_CHECK_INTERVAL, ProcessManager};
use crate::{logging, screen, update};
use anyhow::Context;
use tokio::sync::{broadcast, oneshot};
use tokio::task::{JoinError, JoinHandle};

type UploadTask = JoinHandle<anyhow::Result<()>>;
type UploadResult = Result<anyhow::Result<()>, JoinError>;

/// 协调服务端命令、后台任务和受管进程的生命周期。
pub(crate) struct Runtime {
    logging_config: LoggingConfig,
    process_manager: ProcessManager,
    update_manager: update::UpdateManager,
    update_rx: Option<update::UpdateReceiver>,
    screenshot_task: Option<UploadTask>,
    log_task: Option<UploadTask>,
}

impl Runtime {
    pub(crate) fn new(logging_config: LoggingConfig) -> Self {
        Self {
            logging_config,
            process_manager: ProcessManager::default(),
            update_manager: update::UpdateManager::new(),
            update_rx: None,
            screenshot_task: None,
            log_task: None,
        }
    }

    pub(crate) async fn run_loop(
        &mut self,
        mut rx: broadcast::Receiver<WsMessage>,
        logging: &mut logging::Logging,
    ) -> anyhow::Result<()> {
        let mut check_interval = tokio::time::interval(APP_CHECK_INTERVAL);
        check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let shutdown_signal = tokio::signal::ctrl_c();
        tokio::pin!(shutdown_signal);

        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(msg) => self.handle_message(msg),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!("Message listener lagged and skipped {skipped} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                result = async {
                    match self.update_rx.as_mut() {
                        Some(receiver) => receiver.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_update_result(result, logging)?;
                },
                result = async {
                    match self.screenshot_task.as_mut() {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_screenshot_result(result);
                },
                result = async {
                    match self.log_task.as_mut() {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_log_result(result);
                },
                _ = check_interval.tick() => {
                    if let Err(error) = self.process_manager.check_and_restart() {
                        tracing::error!("Failed to check application: {error:#}");
                    }
                },
                result = &mut shutdown_signal => {
                    return result.context("Failed to listen for Ctrl-C");
                },
            }
        }
    }

    fn handle_message(&mut self, msg: WsMessage) {
        match &msg.cmd {
            WsCmd::Pong => tracing::debug!("Received pong"),
            WsCmd::Config => {
                if let Ok(text) = serde_json::to_string(&msg.data) {
                    tracing::debug!("Received config: {}", text);
                }
            }
            WsCmd::Apps => match parse_active_app(msg) {
                Ok(selected) => {
                    if let Err(error) = self.process_manager.apply_active_app(selected) {
                        tracing::error!("Failed to apply app config: {error:#}");
                    }
                }
                Err(error) => {
                    tracing::error!("Failed to parse app config: {error:#}");
                }
            },
            WsCmd::Update => {
                if let Some(receiver) = self.update_manager.dispatch(msg) {
                    self.update_rx = Some(receiver);
                }
            }
            WsCmd::Log => {
                if self.log_task.is_some() {
                    tracing::info!("Skipped log command while upload is in progress");
                } else {
                    self.log_task =
                        Some(tokio::spawn(logging::upload(self.logging_config.clone())));
                }
            }
            WsCmd::Screenshot => {
                if self.screenshot_task.is_some() {
                    tracing::info!("Skipped screenshot command while upload is in progress");
                } else {
                    self.screenshot_task = Some(tokio::spawn(screen::screenshot()));
                }
            }
            _ => {
                if let Ok(text) = serde_json::to_string(&msg) {
                    tracing::debug!("Unknown msg: {}", text);
                }
            }
        }
    }

    fn handle_update_result(
        &mut self,
        result: Result<update::UpdateResult, oneshot::error::RecvError>,
        logging: &mut logging::Logging,
    ) -> anyhow::Result<()> {
        self.update_rx = None;
        self.update_manager
            .handle_result(result, &mut self.process_manager, || {
                tracing::info!("Stopped rsd");
                logging.shutdown();
            })
    }

    fn handle_screenshot_result(&mut self, result: UploadResult) {
        self.screenshot_task = None;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!("Failed to upload screenshots: {error:#}"),
            Err(error) => tracing::error!("Screenshot task failed: {error}"),
        }
    }

    fn handle_log_result(&mut self, result: UploadResult) {
        self.log_task = None;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!("Failed to upload logs: {error:#}"),
            Err(error) => tracing::error!("Log upload task failed: {error}"),
        }
    }

    pub(crate) async fn shutdown(&mut self) {
        if let Some(task) = self.screenshot_task.take() {
            task.abort();
        }
        if let Some(task) = self.log_task.take() {
            task.abort();
            let _ = task.await;
        }

        if let Err(error) = self.process_manager.shutdown() {
            tracing::warn!(%error, "Failed to stop applications");
        }

        tracing::info!("Stopped rsd");
    }
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
