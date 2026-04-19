use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tauri::AppHandle;

use crate::app_state::manager_tx;
use crate::identity::app_dir;
use crate::types::{ManagerCmd, RoomConfig, RoomStatusResp, APP_TAG};

const ROOM_FILE: &str = "room.json";

pub fn room_config_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(app_dir(app)?.join(ROOM_FILE))
}

pub fn load_room_config(app: &AppHandle) -> Option<RoomConfig> {
    let path = room_config_path(app).ok()?;
    if !path.exists() {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice::<RoomConfig>(&bytes).ok()
}

pub fn save_room_config(
    app: &AppHandle,
    cfg: &RoomConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = room_config_path(app)?;
    let json = serde_json::to_vec_pretty(cfg)?;
    fs::write(&path, json)?;
    Ok(())
}

pub fn delete_room_config(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let path = room_config_path(app)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Dérive topic gossipsub et clé symétrique ChaCha20 à partir du code de salon :
/// - clé = `SHA256("lan-chat-v1|key|" + code)`
/// - topic = `"lan-chat-v1-" + hex(SHA256("lan-chat-v1|topic|" + code))[..16]`
pub fn derive_room(code: &str) -> (String, [u8; 32]) {
    let mut hk = Sha256::new();
    hk.update(b"lan-chat-v1|key|");
    hk.update(code.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&hk.finalize());

    let mut ht = Sha256::new();
    ht.update(b"lan-chat-v1|topic|");
    ht.update(code.as_bytes());
    let topic = format!("{}-{}", APP_TAG, hex::encode(&ht.finalize()[..8]));

    (topic, key)
}

#[tauri::command]
pub fn get_room_status(app: AppHandle) -> RoomStatusResp {
    let cfg = load_room_config(&app);
    RoomStatusResp {
        configured: cfg.is_some(),
        room_name: cfg.map(|c| c.code),
    }
}

#[tauri::command]
pub fn set_room_code(app: AppHandle, code: String) -> Result<(), String> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return Err("Le code de salon ne peut pas être vide.".into());
    }
    if trimmed.len() > 120 {
        return Err("Code de salon trop long (max 120 caractères).".into());
    }

    let config = RoomConfig {
        version: 1,
        code: trimmed.to_string(),
    };
    save_room_config(&app, &config).map_err(|e| format!("Écriture de room.json : {}", e))?;

    manager_tx()
        .get()
        .ok_or_else(|| "Manager non initialisé".to_string())?
        .send(ManagerCmd::StartRoom(config))
        .map_err(|e| format!("Canal manager fermé : {}", e))?;

    // Pas de set_node_status ici : le manager transitionne Initializing→Ready lui-même,
    // et toucher le status ici pourrait clobber un Ready déjà posé (race avec node-ready event).
    Ok(())
}

#[tauri::command]
pub fn leave_room(app: AppHandle) -> Result<(), String> {
    delete_room_config(&app).map_err(|e| format!("Suppression de room.json : {}", e))?;
    manager_tx()
        .get()
        .ok_or_else(|| "Manager non initialisé".to_string())?
        .send(ManagerCmd::LeaveRoom)
        .map_err(|e| format!("Canal manager fermé : {}", e))?;
    Ok(())
}

/// Alias legacy : d'anciens frontends appelaient `reset_room` (équivalent de `leave_room`).
#[tauri::command]
pub fn reset_room(app: AppHandle) -> Result<(), String> {
    leave_room(app)
}
