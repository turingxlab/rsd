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

use crate::network::protocol::{Application, Launcher};
use anyhow::{Context, Result};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// 当前激活应用的进程存活检查间隔
pub(crate) const APP_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// 最大重试次数
const MAX_RESTARTS: u32 = 5;
/// 稳定运行持续时间超过这个值，就认为应用已稳定，重置重试次数
const STABLE_RUN_DURATION: Duration = Duration::from_secs(60);

enum ProcessStatus {
    Running(Duration),
    NotRunning(Option<ExitStatus>),
}

/// 保存应用目标；即使启动失败，也保留配置以便后续重试。
struct ProcessRecord {
    application: Application,
    launcher: Launcher,
    child: Option<Child>,
    started_at: Option<Instant>,
    restart_attempts: u32,
    restart_exhausted: bool,
}

impl ProcessRecord {
    fn same_launch_config(&self, other_app: &Application, launcher: &Launcher) -> bool {
        let app = &self.application;
        app.app_id == other_app.app_id
            && app.app_type == other_app.app_type
            && app.url == other_app.url
            && app.args == other_app.args
            && app.version == other_app.version
            && app.launcher_id == other_app.launcher_id
            && self.launcher.launcher_id == launcher.launcher_id
            && self.launcher.path == launcher.path
            && self.launcher.version == launcher.version
    }
}

/// 由消息循环独占，管理唯一的应用进程及其重启状态。
#[derive(Default)]
pub(crate) struct ProcessManager {
    process: Option<ProcessRecord>,
}

impl ProcessManager {
    /// 应用有效配置；重复下发会重置重启次数，启动配置未变则保留存活进程。
    pub(crate) fn apply_active_app(
        &mut self,
        selected: Option<(Application, Launcher)>,
    ) -> Result<()> {
        let Some((application, launcher)) = selected else {
            return self.shutdown();
        };
        let unchanged = self
            .process
            .as_ref()
            .is_some_and(|process| process.same_launch_config(&application, &launcher));

        if !unchanged {
            // 停止失败时保留原进程和原目标，避免失去进程管理权。
            self.stop_process()?;
        }
        if unchanged {
            let process = self.process.as_mut().unwrap();
            process.application = application;
            process.launcher = launcher;
            process.restart_attempts = 0;
            process.restart_exhausted = false;
        } else {
            self.process = Some(ProcessRecord {
                application,
                launcher,
                child: None,
                started_at: None,
                restart_attempts: 0,
                restart_exhausted: false,
            });
        }
        if unchanged && matches!(self.process_status()?, ProcessStatus::Running(_)) {
            return Ok(());
        }
        // 启动失败也保留目标配置，交由定时检查重试。
        self.start_process()
    }

    pub(crate) fn check_and_restart(&mut self) -> Result<()> {
        if self.process.is_none() {
            return Ok(());
        }
        let status = self.process_status()?;
        if let ProcessStatus::Running(uptime) = status {
            let active = self.process.as_mut().unwrap();
            if active.restart_attempts > 0 && uptime >= STABLE_RUN_DURATION {
                active.restart_attempts = 0;
                tracing::info!(
                    "Application {} stayed healthy for {} seconds; restart attempts reset",
                    active.application.app_id,
                    uptime.as_secs()
                );
            }
            return Ok(());
        }

        // 退出状态已由存活检查回收；达到重启次数上限后不再尝试启动。
        let active = self.process.as_mut().unwrap();
        if active.restart_exhausted {
            return Ok(());
        }
        if active.restart_attempts >= MAX_RESTARTS {
            active.restart_exhausted = true;
            tracing::error!(
                "Application {} reached the restart limit ({} attempts); waiting for a new apps config",
                active.application.app_id,
                MAX_RESTARTS
            );
            return Ok(());
        }
        active.restart_attempts += 1;
        match status {
            ProcessStatus::NotRunning(Some(status)) => tracing::warn!(
                "Application {} exited (status: {status}); restarting (attempt {}/{})",
                active.application.app_id,
                active.restart_attempts,
                MAX_RESTARTS
            ),
            _ => tracing::warn!(
                "Application {} is not running; restarting (attempt {}/{})",
                active.application.app_id,
                active.restart_attempts,
                MAX_RESTARTS
            ),
        }
        self.start_process()
    }

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.stop_process()?;
        self.process = None;
        Ok(())
    }

    fn start_process(&mut self) -> Result<()> {
        self.stop_process()?;
        let Some(process) = &mut self.process else {
            return Ok(());
        };
        let application = &process.application;
        let launcher = &process.launcher;
        let child = Command::new(&launcher.path)
            .arg(&application.url)
            .args(&application.args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to start application {}: {}",
                    application.app_id, launcher.path
                )
            })?;
        tracing::info!(
            "Started {} v{}, AppId: {}",
            application.app_name,
            application.version,
            application.app_id
        );
        process.child = Some(child);
        process.started_at = Some(Instant::now());
        Ok(())
    }

    fn process_status(&mut self) -> Result<ProcessStatus> {
        let Some(process) = &mut self.process else {
            return Ok(ProcessStatus::NotRunning(None));
        };
        let Some(child) = &mut process.child else {
            return Ok(ProcessStatus::NotRunning(None));
        };
        if let Some(status) = child.try_wait()? {
            process.child = None;
            process.started_at = None;
            return Ok(ProcessStatus::NotRunning(Some(status)));
        }
        Ok(ProcessStatus::Running(
            process
                .started_at
                .context("Running process is missing its start time")?
                .elapsed(),
        ))
    }

    fn stop_process(&mut self) -> Result<()> {
        let Some(process) = &mut self.process else {
            return Ok(());
        };
        let Some(child) = &mut process.child else {
            return Ok(());
        };
        match child.try_wait()? {
            Some(status) => tracing::info!(
                "Application {} exited (status: {status})",
                process.application.app_id
            ),
            None => {
                child.kill()?;
                child.wait()?;
                tracing::info!(
                    "Stopped {} v{}, AppId: {}",
                    process.application.app_name,
                    process.application.version,
                    process.application.app_id
                );
            }
        }
        process.child = None;
        process.started_at = None;
        Ok(())
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::warn!("Failed to clean up application: {error:#}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::network::protocol::AppType;

    fn active_app(id: &str) -> (Application, Launcher) {
        (
            Application {
                app_id: id.to_owned(),
                app_name: id.to_owned(),
                app_type: AppType::Browser,
                version: String::new(),
                url: "30".to_owned(),
                args: Vec::new(),
                launcher_id: "sleep".to_owned(),
                active: true,
            },
            Launcher {
                launcher_id: "sleep".to_owned(),
                path: "/bin/sleep".to_owned(),
                version: String::new(),
            },
        )
    }

    fn started_manager() -> ProcessManager {
        let mut manager = ProcessManager::default();
        manager
            .apply_active_app(Some(active_app("active")))
            .unwrap();
        manager
    }

    fn exit_process(manager: &mut ProcessManager) {
        let child = manager.process.as_mut().unwrap().child.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn switching_reaps_old_process_and_none_stops_supervision() {
        let mut manager = started_manager();
        manager.apply_active_app(Some(active_app("next"))).unwrap();
        assert_eq!(manager.process.as_ref().unwrap().application.app_id, "next");
        manager.apply_active_app(None).unwrap();
        manager.check_and_restart().unwrap();
        assert!(manager.process.is_none());
        manager.shutdown().unwrap();
    }

    #[test]
    fn repeated_config_preserves_pid_and_updates_display_fields() {
        let mut manager = started_manager();
        let pid = manager
            .process
            .as_ref()
            .unwrap()
            .child
            .as_ref()
            .unwrap()
            .id();
        let mut config = active_app("active");
        config.0.app_name = "Renamed".to_owned();
        manager.apply_active_app(Some(config)).unwrap();
        let process = manager.process.as_ref().unwrap();
        assert_eq!(process.child.as_ref().unwrap().id(), pid);
        assert_eq!(process.application.app_name, "Renamed");
    }

    #[test]
    fn changing_launch_fields_replaces_process() {
        let original = active_app("active");
        let mut manager = started_manager();
        let changes: [fn(&mut (Application, Launcher)); 8] = [
            |app| app.0.app_id = "other".to_owned(),
            |app| app.0.url = "20".to_owned(),
            |app| app.0.args.push("--help".to_owned()),
            |app| app.0.version = "2".to_owned(),
            |app| app.0.launcher_id = "other".to_owned(),
            |app| app.1.launcher_id = "other".to_owned(),
            |app| app.1.path = "/usr/bin/true".to_owned(),
            |app| app.1.version = "2".to_owned(),
        ];
        for change in changes {
            let mut config = original.clone();
            change(&mut config);
            assert!(
                !manager
                    .process
                    .as_ref()
                    .unwrap()
                    .same_launch_config(&config.0, &config.1)
            );
        }

        let mut config = original;
        config.0.url = "20".to_owned();
        manager.apply_active_app(Some(config)).unwrap();
        assert_eq!(manager.process.as_ref().unwrap().application.url, "20");
    }

    #[test]
    fn exited_process_is_restarted_and_reaped() {
        let mut manager = started_manager();
        exit_process(&mut manager);
        assert!(matches!(
            manager.process_status().unwrap(),
            ProcessStatus::NotRunning(Some(_))
        ));
        let process = manager.process.as_ref().unwrap();
        assert!(process.child.is_none());
        assert!(process.started_at.is_none());
        assert_eq!(process.application.app_id, "active");
        assert_eq!(process.restart_attempts, 0);
        assert!(matches!(
            manager.process_status().unwrap(),
            ProcessStatus::NotRunning(None)
        ));
        manager.check_and_restart().unwrap();
        assert!(matches!(
            manager.process_status().unwrap(),
            ProcessStatus::Running(_)
        ));
        assert_eq!(manager.process.as_ref().unwrap().restart_attempts, 1);
    }

    #[test]
    fn failed_starts_consume_exactly_five_retries_and_config_resets_attempts() {
        let mut manager = ProcessManager::default();
        let mut config = active_app("failed");
        config.1.path = "/rsd-test-missing-launcher".to_owned();
        assert!(manager.apply_active_app(Some(config.clone())).is_err());
        assert!(manager.process.as_ref().unwrap().child.is_none());
        assert!(manager.process.as_ref().unwrap().started_at.is_none());
        for attempt in 1..=MAX_RESTARTS {
            assert!(manager.check_and_restart().is_err());
            assert_eq!(manager.process.as_ref().unwrap().restart_attempts, attempt);
        }
        for _ in 0..2 {
            manager.check_and_restart().unwrap();
            assert!(manager.process.as_ref().unwrap().restart_exhausted);
            assert!(manager.process.as_ref().unwrap().child.is_none());
        }
        assert!(manager.apply_active_app(Some(config)).is_err());
        let active = manager.process.as_ref().unwrap();
        assert_eq!(active.restart_attempts, 0);
        assert!(!active.restart_exhausted);
        assert!(manager.check_and_restart().is_err());
    }

    #[test]
    fn fifth_restart_can_run_and_recover_after_sixty_seconds() {
        let mut manager = started_manager();
        manager.process.as_mut().unwrap().restart_attempts = MAX_RESTARTS - 1;
        exit_process(&mut manager);
        manager.check_and_restart().unwrap();
        assert_eq!(
            manager.process.as_ref().unwrap().restart_attempts,
            MAX_RESTARTS
        );
        manager.check_and_restart().unwrap();
        assert!(!manager.process.as_ref().unwrap().restart_exhausted);

        manager.process.as_mut().unwrap().started_at = Some(Instant::now() - STABLE_RUN_DURATION);
        manager.check_and_restart().unwrap();
        assert_eq!(manager.process.as_ref().unwrap().restart_attempts, 0);
    }

    #[test]
    fn short_run_does_not_reset_attempts_and_final_exit_reaches_limit() {
        let mut manager = started_manager();
        manager.process.as_mut().unwrap().restart_attempts = MAX_RESTARTS;
        manager.check_and_restart().unwrap();
        assert_eq!(
            manager.process.as_ref().unwrap().restart_attempts,
            MAX_RESTARTS
        );
        exit_process(&mut manager);
        manager.check_and_restart().unwrap();
        assert!(manager.process.as_ref().unwrap().restart_exhausted);
        assert!(manager.process.as_ref().unwrap().child.is_none());
        assert!(manager.process.as_ref().unwrap().started_at.is_none());
    }

    #[test]
    fn shutdown_reaps_exited_process() {
        let mut manager = started_manager();
        exit_process(&mut manager);
        manager.shutdown().unwrap();
        assert!(manager.process.is_none());
    }
}
