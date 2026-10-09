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
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

struct ProcessRecord {
    child: Child,
    application: Application,
}

#[derive(Clone, Default)]
pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ProcessRecord>>>,
}

impl ProcessManager {
    pub fn start(&self, application: &Application, launcher: &Launcher) -> Result<()> {
        {
            let mut processes = self
                .processes
                .lock()
                .map_err(|_| anyhow!("Process manager lock poisoned"))?;
            if let Some(process) = processes.get_mut(&application.app_id) {
                if process.child.try_wait()?.is_none() {
                    return Ok(());
                }
            }
        }
        self.stop_all()?;
        let mut command = Command::new(&launcher.path);
        command
            .arg(&application.url)
            .args(&application.args)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().with_context(|| {
            format!(
                "Failed to start application {}: {}",
                application.app_id, launcher.path
            )
        })?;
        self.processes
            .lock()
            .map_err(|_| anyhow!("Process manager lock poisoned"))?
            .insert(
                application.app_id.clone(),
                ProcessRecord {
                    child,
                    application: application.clone(),
                },
            );
        tracing::info!(
            "Started {} v{}, AppId: {}",
            application.app_name,
            application.version,
            application.app_id
        );
        Ok(())
    }

    pub fn stop(&self, app_id: &str) -> Result<()> {
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| anyhow!("Process manager lock poisoned"))?;
        let Some(process) = processes.get_mut(app_id) else {
            return Ok(());
        };
        if process.child.try_wait()?.is_none() {
            process.child.kill()?;
            process.child.wait()?;
        }
        if let Some(process) = processes.remove(app_id) {
            tracing::info!(
                "Stopped {} v{}, AppId: {}",
                process.application.app_name,
                process.application.version,
                process.application.app_id
            );
        }
        Ok(())
    }

    pub fn stop_all(&self) -> Result<()> {
        self.stop_except(None)
    }

    pub fn stop_except(&self, keep: Option<&str>) -> Result<()> {
        let ids: Vec<String> = self
            .processes
            .lock()
            .map_err(|_| anyhow!("Process manager lock poisoned"))?
            .keys()
            .filter(|id| Some(id.as_str()) != keep)
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Err(error) = self.stop(&id) {
                errors.push(format!("{error:#}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("Failed to stop processes: {}", errors.join("; ")))
        }
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.processes) == 1 {
            if let Err(error) = self.stop_all() {
                tracing::warn!("Failed to clean up processes {}", error);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::network::protocol::AppType;

    fn application(id: &str) -> Application {
        Application {
            app_id: id.to_owned(),
            app_name: id.to_owned(),
            app_type: AppType::Browser,
            version: String::new(),
            url: "30".to_owned(),
            args: Vec::new(),
            launcher_id: "sleep".to_owned(),
            active: true,
        }
    }

    fn launcher() -> Launcher {
        Launcher {
            launcher_id: "sleep".to_owned(),
            path: "/bin/sleep".to_owned(),
            version: String::new(),
        }
    }

    #[test]
    fn start_replaces_existing_application_and_uses_configured_id() {
        let manager = ProcessManager::default();
        manager.start(&application("first"), &launcher()).unwrap();
        let mut second = application("second");
        second.app_name = "Second application".to_owned();
        second.version = "1.2.3".to_owned();
        manager.start(&second, &launcher()).unwrap();
        second.app_name = "Updated application".to_owned();
        second.version = "2.0.0".to_owned();
        let processes = manager.processes.lock().unwrap();
        assert_eq!(processes.len(), 1);
        let stored = &processes.get("second").unwrap().application;
        assert_eq!(stored.app_id, "second");
        assert_eq!(stored.app_name, "Second application");
        assert_eq!(stored.version, "1.2.3");
        drop(processes);
        manager.stop_all().unwrap();
    }

    #[test]
    fn start_preserves_an_already_running_active_application() {
        let manager = ProcessManager::default();
        let application = application("active");
        manager.start(&application, &launcher()).unwrap();
        let original_pid = manager.processes.lock().unwrap()["active"].child.id();

        manager.stop_except(Some("active")).unwrap();
        manager.start(&application, &launcher()).unwrap();

        let mut processes = manager.processes.lock().unwrap();
        assert_eq!(processes.len(), 1);
        let process = processes.get_mut("active").unwrap();
        assert_eq!(process.child.id(), original_pid);
        assert!(process.child.try_wait().unwrap().is_none());
        drop(processes);
        manager.stop_all().unwrap();
    }

    #[test]
    fn start_restarts_an_already_exited_application() {
        let manager = ProcessManager::default();
        let application = application("exited");
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        manager.processes.lock().unwrap().insert(
            application.app_id.clone(),
            ProcessRecord {
                child,
                application: application.clone(),
            },
        );

        manager.stop_except(Some("exited")).unwrap();
        manager.start(&application, &launcher()).unwrap();

        let mut processes = manager.processes.lock().unwrap();
        assert_eq!(processes.len(), 1);
        assert!(
            processes
                .get_mut("exited")
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        drop(processes);
        manager.stop_all().unwrap();
    }

    #[test]
    fn stop_except_and_stop_all_clean_up_multiple_children() {
        let manager = ProcessManager::default();
        for id in ["keep", "removed", "inactive"] {
            let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            manager.processes.lock().unwrap().insert(
                id.to_owned(),
                ProcessRecord {
                    child,
                    application: application(id),
                },
            );
        }
        manager.stop_except(Some("keep")).unwrap();
        assert_eq!(manager.processes.lock().unwrap().len(), 1);
        manager.stop_all().unwrap();
        manager.stop_all().unwrap();
        assert!(manager.processes.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_handles_an_already_exited_child() {
        let manager = ProcessManager::default();
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        manager.processes.lock().unwrap().insert(
            "exited".to_owned(),
            ProcessRecord {
                child,
                application: application("exited"),
            },
        );
        manager.stop_all().unwrap();
        assert!(manager.processes.lock().unwrap().is_empty());
    }

    #[test]
    fn start_failure_does_not_register_a_process() {
        let manager = ProcessManager::default();
        let mut launcher = launcher();
        launcher.path = String::new();
        assert!(manager.start(&application("failed"), &launcher).is_err());
        assert!(manager.processes.lock().unwrap().is_empty());
    }
}
