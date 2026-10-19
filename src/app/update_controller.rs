//! Desktop-only confirmation and progress state over the installer backend.
use super::{
    update_gate,
    updater::{self, PreparedUpdate, Release, UpdateStatus},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCommand {
    DownloadAndInstall,
    Cancel,
    Retry,
    RestartNow,
    RestartLater,
}
enum Message {
    Status(UpdateStatus),
    Prepared(Result<PreparedUpdate, String>),
    Installed(
        Result<updater::InstallOutcome, String>,
        update_gate::InstallGuard,
    ),
}
pub struct UpdateController {
    pub status: Option<UpdateStatus>,
    pub installer_handoff: bool,
    pub restart_later: bool,
    rx: Option<mpsc::Receiver<Message>>,
    cancel: Arc<AtomicBool>,
    prepared: Option<PreparedUpdate>,
    installation: Option<update_gate::InstallGuard>,
    pending: Option<update_gate::PendingInstall>,
    pub install_requested: bool,
}
impl Default for UpdateController {
    fn default() -> Self {
        Self {
            status: None,
            installer_handoff: false,
            restart_later: false,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            prepared: None,
            installation: None,
            pending: None,
            install_requested: false,
        }
    }
}
impl UpdateController {
    pub fn begin(&mut self, ctx: &egui::Context) {
        if self.rx.is_some() || self.installation.is_some() {
            return;
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        self.status = Some(UpdateStatus::Downloading {
            bytes: 0,
            total: None,
        });
        self.install_requested = true;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let emit = |status| {
                let _ = tx.send(Message::Status(status));
                ctx.request_repaint();
            };
            let result = (|| {
                let client = reqwest::blocking::Client::builder()
                    .user_agent(format!("inxm-local/{}", crate::version::DISPLAY))
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| e.to_string())?;
                let response = client
                    .get("https://api.github.com/repos/inxm-ai/inxm-local/releases/latest")
                    .send()
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| e.to_string())?;
                let body = response
                    .json::<serde_json::Value>()
                    .map_err(|e| e.to_string())?;
                let release = Release::from_github_value(&body)?;
                if !super::engine::is_newer_version(&release.version, crate::version::DISPLAY) {
                    return Err(
                        "The stable release is not newer than this installation.".to_owned()
                    );
                }
                updater::download_and_stage(release, cancel, emit)
            })();
            let _ = tx.send(Message::Prepared(result));
            ctx.request_repaint();
        });
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.install_requested = false;
        self.pending = None;
        self.prepared = None;
        if self.rx.is_none() {
            self.status = Some(UpdateStatus::Cancelled);
        }
    }
    pub fn poll(&mut self) {
        let messages = self
            .rx
            .as_ref()
            .map(|rx| rx.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for message in messages {
            match message {
                Message::Status(status) => self.status = Some(status),
                Message::Prepared(result) => {
                    self.rx = None;
                    match result {
                        Ok(prepared) if !self.cancel.load(Ordering::Relaxed) => {
                            self.prepared = Some(prepared)
                        }
                        Ok(_) => self.status = Some(UpdateStatus::Cancelled),
                        Err(error) => {
                            self.install_requested = false;
                            self.status = Some(if self.cancel.load(Ordering::Relaxed) {
                                UpdateStatus::Cancelled
                            } else {
                                UpdateStatus::Failed(error)
                            });
                        }
                    }
                }
                Message::Installed(result, guard) => {
                    self.rx = None;
                    match result {
                        Ok(outcome) => {
                            self.installer_handoff = outcome.installer_handoff;
                            self.status = Some(UpdateStatus::Installed {
                                restart_required: outcome.restart_required,
                            });
                            self.installation = Some(guard);
                        }
                        Err(error) => {
                            self.status = Some(UpdateStatus::Failed(error));
                        }
                    }
                }
            }
        }
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn ready_to_install(&self) -> bool {
        self.install_requested && self.prepared.is_some()
    }
    pub fn install(&mut self, data_dir: &std::path::Path, ctx: &egui::Context) {
        if self.pending.is_none() {
            match update_gate::begin(data_dir) {
                Ok(pending) => self.pending = Some(pending),
                Err(error) => {
                    self.failure(error);
                    return;
                }
            }
        }
        let work = match self.pending.as_ref().unwrap().quiescent(data_dir) {
            Ok(work) => work,
            Err(error) => {
                self.failure(error);
                return;
            }
        };
        let active = crate::storage::StorageRoot::open(data_dir)
            .and_then(|storage| storage.runs().list())
            .map(|runs| {
                runs.iter()
                    .any(|run| run.status == crate::executor::RunStatus::Running)
            });
        if !matches!(active, Ok(false)) {
            self.status = Some(UpdateStatus::Failed("Active runs remain or could not be checked. Installation deferred; retry when they finish.".to_owned()));
            self.install_requested = false;
            return;
        }
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let guard = self.pending.take().unwrap().acquire(work);
        self.install_requested = false;
        self.status = Some(UpdateStatus::Installing);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = prepared.install();
            let _ = tx.send(Message::Installed(result, guard));
            ctx.request_repaint();
        });
    }
    pub fn retry(&mut self, ctx: &egui::Context) {
        if self.prepared.is_some() {
            self.install_requested = true;
        } else {
            self.begin(ctx);
        }
    }
    pub fn failure(&mut self, error: String) {
        self.install_requested = false;
        self.status = Some(UpdateStatus::Failed(error));
    }
}
