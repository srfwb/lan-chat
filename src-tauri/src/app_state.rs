use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc;

use crate::files::{DownloadState, ServedFile, SwarmFileCmd};
use crate::history::HistoryStore;
use crate::types::{ChatMessage, FileOffer, ManagerCmd, NodeStatus, WireMessage};

static STATE: OnceLock<Mutex<NodeStatus>> = OnceLock::new();
static HISTORY: OnceLock<Mutex<Option<VecDeque<ChatMessage>>>> = OnceLock::new();
static HISTORY_STORE: OnceLock<Mutex<Option<Arc<HistoryStore>>>> = OnceLock::new();
static PUBLISHER_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> = OnceLock::new();
static MANAGER_TX: OnceLock<mpsc::UnboundedSender<ManagerCmd>> = OnceLock::new();
static SERVED_FILES: OnceLock<Mutex<HashMap<String, ServedFile>>> = OnceLock::new();
static RECEIVED_OFFERS: OnceLock<Mutex<HashMap<String, FileOffer>>> = OnceLock::new();
static ACTIVE_DOWNLOADS: OnceLock<Mutex<HashMap<String, DownloadState>>> = OnceLock::new();
static FILE_CMD_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<SwarmFileCmd>>>> = OnceLock::new();

pub fn node_state() -> &'static Mutex<NodeStatus> {
    STATE.get_or_init(|| Mutex::new(NodeStatus::Initializing))
}

pub fn set_node_status(status: NodeStatus) {
    if let Ok(mut guard) = node_state().lock() {
        *guard = status;
    }
}

pub fn history_cell() -> &'static Mutex<Option<VecDeque<ChatMessage>>> {
    HISTORY.get_or_init(|| Mutex::new(None))
}

pub fn history_store_cell() -> &'static Mutex<Option<Arc<HistoryStore>>> {
    HISTORY_STORE.get_or_init(|| Mutex::new(None))
}

pub fn publisher_tx_cell() -> &'static Mutex<Option<mpsc::UnboundedSender<WireMessage>>> {
    PUBLISHER_TX.get_or_init(|| Mutex::new(None))
}

pub fn manager_tx() -> &'static OnceLock<mpsc::UnboundedSender<ManagerCmd>> {
    &MANAGER_TX
}

pub fn served_files_cell() -> &'static Mutex<HashMap<String, ServedFile>> {
    SERVED_FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn received_offers_cell() -> &'static Mutex<HashMap<String, FileOffer>> {
    RECEIVED_OFFERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn active_downloads_cell() -> &'static Mutex<HashMap<String, DownloadState>> {
    ACTIVE_DOWNLOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn file_cmd_tx_cell() -> &'static Mutex<Option<mpsc::UnboundedSender<SwarmFileCmd>>> {
    FILE_CMD_TX.get_or_init(|| Mutex::new(None))
}

/// Vide toutes les ressources liées à un salon (appelée avant chaque StartRoom/LeaveRoom).
/// Le swarm précédent a été aborté juste avant ; son Drop ferme TCP/mDNS.
pub fn clear_shared_state() {
    if let Ok(mut g) = history_cell().lock() {
        *g = None;
    }
    if let Ok(mut g) = history_store_cell().lock() {
        *g = None;
    }
    if let Ok(mut g) = publisher_tx_cell().lock() {
        *g = None;
    }
    if let Ok(mut g) = file_cmd_tx_cell().lock() {
        *g = None;
    }
    if let Ok(mut g) = served_files_cell().lock() {
        g.clear();
    }
    if let Ok(mut g) = received_offers_cell().lock() {
        g.clear();
    }
    if let Ok(mut g) = active_downloads_cell().lock() {
        for (_, state) in g.iter() {
            let _ = fs::remove_file(&state.temp_path);
        }
        g.clear();
    }
}
