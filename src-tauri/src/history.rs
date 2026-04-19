use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::{history_cell, history_store_cell, publisher_tx_cell};
use crate::crypto::{decrypt_chat_message, encrypt_chat_message};
use crate::types::{ChatMessage, EncryptedEnvelope, WireMessage, APP_TAG};

const HISTORY_CAP: usize = 500;
const HISTORY_FILE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HistoryRequest {
    pub version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HistoryResponse {
    pub version: u32,
    pub messages: Vec<EncryptedEnvelope>,
}

pub struct HistoryStore {
    pub path: PathBuf,
    key: [u8; 32],
}

#[derive(Serialize)]
struct HistoryFilePayloadOut<'a> {
    version: u32,
    messages: &'a VecDeque<ChatMessage>,
}

#[derive(Deserialize)]
struct HistoryFilePayloadIn {
    #[allow(dead_code)]
    version: u32,
    messages: Vec<ChatMessage>,
}

impl HistoryStore {
    pub fn new(path: PathBuf, key: [u8; 32]) -> Self {
        Self { path, key }
    }

    fn cipher(&self) -> Result<ChaCha20Poly1305, String> {
        ChaCha20Poly1305::new_from_slice(&self.key).map_err(|e| e.to_string())
    }

    /// Format disque : nonce[0..12] ++ ct (tolérant aux erreurs — renvoie Vec vide).
    pub fn load(&self) -> Vec<ChatMessage> {
        if !self.path.exists() {
            return Vec::new();
        }
        let cipher = match self.cipher() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[lan-chat] History cipher init error: {}", e);
                return Vec::new();
            }
        };
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[lan-chat] History read error: {}", e);
                return Vec::new();
            }
        };
        if bytes.len() < 12 {
            eprintln!("[lan-chat] History file too small (<12 bytes)");
            return Vec::new();
        }
        let (nonce_bytes, ct) = bytes.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = match cipher.decrypt(nonce, ct) {
            Ok(p) => p,
            Err(_) => {
                eprintln!("[lan-chat] History decrypt failed (wrong key or tampered file)");
                return Vec::new();
            }
        };
        match serde_json::from_slice::<HistoryFilePayloadIn>(&plaintext) {
            Ok(p) => p.messages,
            Err(e) => {
                eprintln!("[lan-chat] History parse error: {}", e);
                Vec::new()
            }
        }
    }

    /// Écriture atomique : tmp + rename.
    pub fn save(&self, messages: &VecDeque<ChatMessage>) -> Result<(), String> {
        let cipher = self.cipher()?;
        let payload = HistoryFilePayloadOut {
            version: HISTORY_FILE_VERSION,
            messages,
        };
        let json = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;

        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, json.as_slice())
            .map_err(|e| format!("encrypt history: {}", e))?;

        let mut file_bytes = Vec::with_capacity(12 + ct.len());
        file_bytes.extend_from_slice(&nonce_bytes);
        file_bytes.extend_from_slice(&ct);

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("bin.tmp");
        fs::write(&tmp, &file_bytes).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        if self.path.exists() {
            fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub fn history_path_for(app_data: &Path, topic: &str) -> PathBuf {
    let prefix = format!("{}-", APP_TAG);
    let hash_part = topic.strip_prefix(&prefix).unwrap_or(topic);
    app_data.join("history").join(format!("{}.bin", hash_part))
}

/// Ajoute un message à l'historique en mémoire et persiste (best-effort).
/// Retourne `true` si nouveau, `false` si déjà présent (dédup par id) ou stores pas initialisés.
/// Les deux cas `false` sans log étaient silencieux : on logue maintenant les échecs d'init
/// et de lock pour détecter les races d'ordonnancement (ex : message envoyé pendant la
/// fenêtre entre StartRoom et l'init de `history_cell` dans `run_swarm`).
pub fn append_to_history_if_new(msg: ChatMessage) -> bool {
    let snapshot = {
        let mut guard = match history_cell().lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!(
                    "[lan-chat] History cell lock empoisonné — message {} non archivé",
                    msg.id
                );
                return false;
            }
        };
        let hist = match guard.as_mut() {
            Some(h) => h,
            None => {
                eprintln!(
                    "[lan-chat] History cell non initialisée — message {} non archivé (race ?)",
                    msg.id
                );
                return false;
            }
        };
        if hist.iter().any(|m| m.id == msg.id) {
            return false;
        }
        hist.push_back(msg);
        while hist.len() > HISTORY_CAP {
            hist.pop_front();
        }
        hist.clone()
    };

    let store = match history_store_cell().lock() {
        Ok(g) => g.as_ref().cloned(),
        Err(_) => None,
    };
    if let Some(store) = store {
        if let Err(e) = store.save(&snapshot) {
            eprintln!("[lan-chat] History save error: {}", e);
        }
    }
    true
}

pub fn append_to_history(msg: ChatMessage) {
    let _ = append_to_history_if_new(msg);
}

pub fn build_history_response(cipher: &ChaCha20Poly1305) -> HistoryResponse {
    let messages: Vec<ChatMessage> = history_cell()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|h| h.iter().cloned().collect()))
        .unwrap_or_default();

    let envelopes: Vec<EncryptedEnvelope> = messages
        .iter()
        .filter_map(|m| encrypt_chat_message(cipher, m))
        .collect();

    HistoryResponse {
        version: 1,
        messages: envelopes,
    }
}

pub fn ingest_history_response(
    cipher: &ChaCha20Poly1305,
    response: HistoryResponse,
    app: &AppHandle,
) {
    if response.version != 1 {
        eprintln!(
            "[lan-chat] Ignoring history-sync response (unsupported version {})",
            response.version
        );
        return;
    }
    let total = response.messages.len();
    let mut ingested = 0usize;
    let mut dup = 0usize;
    let mut decrypt_failed = 0usize;
    for envelope in response.messages {
        match decrypt_chat_message(cipher, &envelope) {
            Some(msg) => {
                if append_to_history_if_new(msg.clone()) {
                    ingested += 1;
                    let _ = app.emit("history-message", msg);
                } else {
                    dup += 1;
                }
            }
            None => decrypt_failed += 1,
        }
    }
    eprintln!(
        "[lan-chat] History ingest: {} received → {} new, {} dup, {} decrypt-failed",
        total, ingested, dup, decrypt_failed
    );
}

#[tauri::command]
pub fn send_message(message: ChatMessage) -> Result<(), String> {
    let tx = {
        let guard = publisher_tx_cell()
            .lock()
            .map_err(|_| "Publisher lock empoisonné".to_string())?;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "Publisher non initialisé".to_string())?
    };
    tx.send(WireMessage::Chat(message.clone()))
        .map_err(|e| format!("Échec d'envoi : {}", e))?;
    // gossipsub ne nous renvoie pas notre propre message : on archive localement.
    append_to_history(message);
    Ok(())
}

#[tauri::command]
pub fn get_history() -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = history_cell()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|h| h.iter().cloned().collect()))
        .unwrap_or_default();
    // La deque peut être désordonnée après un sync P2P (push_back sans tri).
    messages.sort_by_key(|m| m.timestamp);
    messages
}

#[tauri::command]
pub fn clear_history() -> Result<(), String> {
    let store = match history_store_cell().lock() {
        Ok(g) => g.as_ref().cloned(),
        Err(_) => None,
    };
    let store = store.ok_or_else(|| "Store non initialisé".to_string())?;
    if let Ok(mut g) = history_cell().lock() {
        if let Some(h) = g.as_mut() {
            h.clear();
        }
    }
    store.clear()
}
