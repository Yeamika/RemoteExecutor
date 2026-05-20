use crate::websocket;
use anyhow::{anyhow, Result};
use pty_t_core::session::Session;
use pty_t_core::{
    default_shell, CommandSpec, PtyManager, SessionDetail as CoreSessionDetail,
    SessionSummary as CoreSessionSummary, TermSize,
};
use pty_t_protocol::{ClientSummary, ServerText, SessionDetail, SessionSummary};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_tungstenite::tungstenite::Message;

struct ConnectedClient {
    token: u64,
    tx: mpsc::UnboundedSender<Message>,
    peer_addr: SocketAddr,
}

#[derive(Clone)]
pub struct ShellManager {
    core: PtyManager,
    locked: Arc<Mutex<HashSet<String>>>,
    clients: Arc<Mutex<HashMap<String, HashMap<String, ConnectedClient>>>>,
}

impl ShellManager {
    pub fn new(default_command: CommandSpec, default_size: TermSize) -> Self {
        Self {
            core: PtyManager::new(default_command, default_size),
            locked: Arc::new(Mutex::new(HashSet::new())),
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn default_shell(cols: u16, rows: u16) -> Self {
        Self::new(CommandSpec::new(default_shell()), TermSize { cols, rows })
    }

    pub fn core(&self) -> PtyManager {
        self.core.clone()
    }

    pub fn create_pty(
        &self,
        name: impl Into<String>,
        command: CommandSpec,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<Arc<Session>> {
        self.core.create_pty(name, command, cols, rows)
    }

    pub fn create_bash(&self, name: impl Into<String>) -> Result<Arc<Session>> {
        self.core.create_bash(name)
    }

    pub fn remove_pty(&self, pty: &str) -> bool {
        self.clients.lock().unwrap().remove(pty);
        self.locked.lock().unwrap().remove(pty);
        self.core.state().remove_session(pty).is_some()
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        self.core
            .list()
            .into_iter()
            .map(|summary| self.attach_client_details(summary))
            .collect()
    }

    pub fn detail(&self, pty: &str) -> Result<SessionDetail> {
        Ok(self.attach_client_details_to_detail(self.core.detail(pty)?))
    }

    pub fn lock_pty(&self, pty: &str) -> Result<()> {
        let session = self
            .core
            .session(pty)
            .ok_or_else(|| anyhow!("pty {pty} does not exist"))?;
        session.force_controller("0");
        self.locked.lock().unwrap().insert(pty.to_string());
        Ok(())
    }

    pub fn unlock_pty(&self, pty: &str) {
        self.locked.lock().unwrap().remove(pty);
    }

    pub fn is_locked(&self, pty: &str) -> bool {
        self.locked.lock().unwrap().contains(pty)
    }

    pub fn start_websocket(&self, addr: impl Into<String>) -> Result<String> {
        websocket::start_listener(addr.into(), self.clone())
    }

    pub async fn attach_execute(
        &self,
        pty: &str,
        input: impl AsRef<[u8]>,
        collect_for: Duration,
    ) -> Result<Vec<u8>> {
        let mut rx = self.core.subscribe_output(pty)?;
        self.core.send_to_pty(pty, input.as_ref())?;

        let deadline = Instant::now() + collect_for;
        let mut output = Vec::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            match time::timeout(deadline - now, rx.recv()).await {
                Ok(Some(chunk)) => output.extend(chunk),
                Ok(None) | Err(_) => break,
            }
        }
        Ok(output)
    }

    pub fn snapshot(&self, pty: &str) -> Result<Vec<u8>> {
        self.core.snapshot_pty(pty)
    }

    fn attach_client_details(&self, summary: CoreSessionSummary) -> SessionSummary {
        let client_details = self.client_details(&summary.pty);
        SessionSummary {
            pty: summary.pty,
            command: summary.command,
            controller: summary.controller,
            cols: summary.cols,
            rows: summary.rows,
            process_id: summary.process_id,
            created_at: summary.created_at,
            output_history_bytes: summary.output_history_bytes,
            output_history_limit: summary.output_history_limit,
            clients: summary.clients,
            client_details,
        }
    }

    fn attach_client_details_to_detail(&self, detail: CoreSessionDetail) -> SessionDetail {
        let client_details = self.client_details(&detail.pty);
        SessionDetail {
            pty: detail.pty,
            command: detail.command,
            cwd: detail.cwd,
            env: detail.env,
            process_id: detail.process_id,
            created_at: detail.created_at,
            controller: detail.controller,
            cols: detail.cols,
            rows: detail.rows,
            output_history_bytes: detail.output_history_bytes,
            output_history_limit: detail.output_history_limit,
            clients: detail.clients,
            client_details,
            exit_code: detail.exit_code,
        }
    }

    pub(crate) fn register_client(
        &self,
        pty: &str,
        id: String,
        token: u64,
        tx: mpsc::UnboundedSender<Message>,
        peer_addr: SocketAddr,
    ) {
        self.clients
            .lock()
            .unwrap()
            .entry(pty.to_string())
            .or_default()
            .insert(
                id,
                ConnectedClient {
                    token,
                    tx,
                    peer_addr,
                },
            );
    }

    pub(crate) fn remove_client(&self, pty: &str, id: &str, token: u64) {
        let mut clients = self.clients.lock().unwrap();
        let Some(session_clients) = clients.get_mut(pty) else {
            return;
        };
        if session_clients.get(id).map(|client| client.token) == Some(token) {
            session_clients.remove(id);
        }
        if session_clients.is_empty() {
            clients.remove(pty);
        }
    }

    pub(crate) fn client_details(&self, pty: &str) -> Vec<ClientSummary> {
        let clients = self.clients.lock().unwrap();
        let Some(session_clients) = clients.get(pty) else {
            return Vec::new();
        };
        let mut details = session_clients
            .iter()
            .map(|(id, client)| ClientSummary {
                id: id.clone(),
                peer_addr: client.peer_addr.to_string(),
            })
            .collect::<Vec<_>>();
        details.sort_by(|a, b| a.id.cmp(&b.id));
        details
    }

    pub(crate) fn broadcast_meta(&self, pty: &str) {
        let Some(session) = self.core.session(pty) else {
            return;
        };
        let summary = session.summary();
        let clients = self
            .clients
            .lock()
            .unwrap()
            .get(pty)
            .map(|clients| {
                clients
                    .iter()
                    .map(|(id, client)| (id.clone(), client.tx.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for (id, tx) in clients {
            let role = if summary.controller.as_deref() == Some(id.as_str()) {
                "Controller"
            } else {
                "Viewer"
            };
            let msg = ServerText::Meta {
                id: id.clone(),
                pty: summary.pty.clone(),
                role: role.to_string(),
                cols: summary.cols,
                rows: summary.rows,
            };
            if let Ok(text) = serde_json::to_string(&msg) {
                let _ = tx.send(Message::Text(text.into()));
            }
        }
    }
}
