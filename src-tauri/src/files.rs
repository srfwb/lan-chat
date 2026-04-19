use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use chacha20poly1305::ChaCha20Poly1305;
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

use crate::app_state::{
    active_downloads_cell, file_cmd_tx_cell, node_state, publisher_tx_cell, received_offers_cell,
    served_files_cell,
};
use crate::crypto::{decrypt_bytes, encrypt_bytes};
use crate::types::{EncryptedEnvelope, FileOffer, NodeStatus, WireMessage};

pub const FILE_CHUNK_SIZE: u32 = 256 * 1024;
pub const MAX_FILE_SIZE: u64 = 500 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ServedFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct DownloadState {
    pub sender_peer: PeerId,
    pub size: u64,
    pub expected_hash: String,
    pub received_bytes: u64,
    pub temp_path: PathBuf,
    pub final_path: PathBuf,
}

pub enum SwarmFileCmd {
    RequestChunk { peer: PeerId, request: ChunkRequest },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkRequest {
    pub version: u32,
    pub file_id: String,
    pub offset: u64,
    pub length: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChunkResponse {
    Ok {
        file_id: String,
        data: EncryptedEnvelope,
    },
    NotFound,
    Error {
        message: String,
    },
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hash SHA256 en streaming (buffer 256 KiB) — évite de charger le fichier entier en RAM.
pub fn compute_sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Résout les collisions : `foo.pdf` existe → `foo (1).pdf`, puis `foo (2).pdf`, etc.
pub fn dedupe_filename(dir: &Path, filename: &str) -> PathBuf {
    let base = dir.join(filename);
    if !base.exists() {
        return base;
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{}", e)),
        None => (filename.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{} ({}){}", stem, n, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(filename)
}

pub fn send_file_cmd(cmd: SwarmFileCmd) {
    let tx = file_cmd_tx_cell()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().cloned());
    let Some(tx) = tx else {
        eprintln!("[lan-chat] File command channel not initialized");
        return;
    };
    if let Err(e) = tx.send(cmd) {
        eprintln!("[lan-chat] Failed to send file cmd: {}", e);
    }
}

pub fn emit_file_error(app: &AppHandle, file_id: &str, msg: &str) {
    eprintln!("[lan-chat] File error ({}): {}", file_id, msg);
    let _ = app.emit(
        "file-error",
        serde_json::json!({
            "fileId": file_id,
            "message": msg,
        }),
    );
}

pub fn abort_download(file_id: &str) {
    if let Ok(mut g) = active_downloads_cell().lock() {
        g.remove(file_id);
    }
}

pub fn serve_chunk(cipher: &ChaCha20Poly1305, req: &ChunkRequest) -> ChunkResponse {
    if req.version != 1 {
        return ChunkResponse::Error {
            message: format!("unsupported chunk-request version {}", req.version),
        };
    }
    let served = match served_files_cell().lock() {
        Ok(g) => g.get(&req.file_id).cloned(),
        Err(_) => None,
    };
    let Some(served) = served else {
        return ChunkResponse::NotFound;
    };
    let mut file = match fs::File::open(&served.path) {
        Ok(f) => f,
        Err(e) => {
            return ChunkResponse::Error {
                message: format!("open: {}", e),
            }
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(req.offset)) {
        return ChunkResponse::Error {
            message: format!("seek: {}", e),
        };
    }
    let remaining = served.size.saturating_sub(req.offset);
    let to_read = std::cmp::min(req.length as u64, remaining) as usize;
    let mut buf = vec![0u8; to_read];
    if let Err(e) = file.read_exact(&mut buf) {
        return ChunkResponse::Error {
            message: format!("read: {}", e),
        };
    }
    match encrypt_bytes(cipher, &buf) {
        Some(envelope) => ChunkResponse::Ok {
            file_id: req.file_id.clone(),
            data: envelope,
        },
        None => ChunkResponse::Error {
            message: "encrypt failed".into(),
        },
    }
}

pub fn ingest_chunk(
    cipher: &ChaCha20Poly1305,
    file_id: &str,
    envelope: EncryptedEnvelope,
    app: &AppHandle,
) {
    let Some(plaintext) = decrypt_bytes(cipher, &envelope) else {
        emit_file_error(app, file_id, "decrypt failed");
        abort_download(file_id);
        return;
    };

    // Écrire le chunk, maj de l'état, calcul de la suite — tout sous lock, court.
    let (received, total, temp_path, sender_peer, completed) = {
        let mut dl = match active_downloads_cell().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(state) = dl.get_mut(file_id) else {
            return; // téléchargement annulé entre-temps (leave_room, etc.)
        };

        if let Err(e) = OpenOptions::new()
            .create(false)
            .append(true)
            .open(&state.temp_path)
            .and_then(|mut f| f.write_all(&plaintext))
        {
            eprintln!("[lan-chat] File chunk write failed: {}", e);
            let file_id_owned = file_id.to_string();
            let temp = state.temp_path.clone();
            drop(dl); // release avant emit
            let _ = fs::remove_file(&temp);
            emit_file_error(app, &file_id_owned, &format!("write: {}", e));
            abort_download(&file_id_owned);
            return;
        }

        state.received_bytes += plaintext.len() as u64;
        let completed = state.received_bytes >= state.size;
        (
            state.received_bytes,
            state.size,
            state.temp_path.clone(),
            state.sender_peer,
            completed,
        )
    };

    let _ = app.emit(
        "file-progress",
        serde_json::json!({
            "fileId": file_id,
            "received": received,
            "total": total,
        }),
    );

    if completed {
        finalize_download(app, file_id, &temp_path);
    } else {
        let next_offset = received;
        let next_length = std::cmp::min(FILE_CHUNK_SIZE as u64, total - received) as u32;
        let req = ChunkRequest {
            version: 1,
            file_id: file_id.to_string(),
            offset: next_offset,
            length: next_length,
        };
        send_file_cmd(SwarmFileCmd::RequestChunk {
            peer: sender_peer,
            request: req,
        });
    }
}

fn finalize_download(app: &AppHandle, file_id: &str, temp_path: &Path) {
    let actual_hash = match compute_sha256_file(temp_path) {
        Ok(h) => h,
        Err(e) => {
            emit_file_error(app, file_id, &format!("hash compute failed: {}", e));
            let _ = fs::remove_file(temp_path);
            abort_download(file_id);
            return;
        }
    };

    let (expected_hash, final_path) = {
        let dl = match active_downloads_cell().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(state) = dl.get(file_id) else {
            return;
        };
        (state.expected_hash.clone(), state.final_path.clone())
    };

    if actual_hash != expected_hash {
        emit_file_error(app, file_id, "hash mismatch");
        let _ = fs::remove_file(temp_path);
        abort_download(file_id);
        return;
    }

    if let Err(e) = fs::rename(temp_path, &final_path) {
        emit_file_error(app, file_id, &format!("rename: {}", e));
        let _ = fs::remove_file(temp_path);
        abort_download(file_id);
        return;
    }

    eprintln!(
        "[lan-chat] File download complete: {} ({} bytes)",
        final_path.display(),
        expected_hash.len()
    );

    let _ = app.emit(
        "file-completed",
        serde_json::json!({
            "fileId": file_id,
            "localPath": final_path.to_string_lossy(),
        }),
    );

    if let Ok(mut g) = active_downloads_cell().lock() {
        g.remove(file_id);
    }
}

#[tauri::command]
pub fn offer_file(
    _app: AppHandle,
    path: String,
    file_id: String,
    sender_name: String,
) -> Result<FileOffer, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("Le chemin n'est pas un fichier : {}", path));
    }
    let metadata = fs::metadata(&src).map_err(|e| format!("Lecture metadata : {}", e))?;
    let size = metadata.len();
    if size == 0 {
        return Err("Fichier vide.".into());
    }
    if size > MAX_FILE_SIZE {
        return Err(format!(
            "Fichier trop gros ({} octets, max {} octets).",
            size, MAX_FILE_SIZE
        ));
    }

    let filename = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin")
        .to_string();

    // Besoin du PeerId local pour signer l'offer — lu depuis le node_state.
    let sender_id = match node_state().lock().ok().map(|g| g.clone()) {
        Some(NodeStatus::Ready { peer_id, .. }) => peer_id,
        _ => return Err("Nœud P2P pas encore prêt.".into()),
    };

    let mime = mime_guess::from_path(&src)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    eprintln!("[lan-chat] Hashing file for offer: {}", src.display());
    let hash = compute_sha256_file(&src)?;

    let offer = FileOffer {
        id: file_id.clone(),
        sender_id,
        sender_name,
        timestamp: now_unix_ms(),
        filename: filename.clone(),
        size,
        mime,
        hash: hash.clone(),
    };

    served_files_cell()
        .lock()
        .map_err(|_| "lock poisoned")?
        .insert(
            file_id.clone(),
            ServedFile {
                path: src.clone(),
                size,
            },
        );

    let tx = {
        let guard = publisher_tx_cell()
            .lock()
            .map_err(|_| "Publisher lock empoisonné".to_string())?;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| "Publisher non initialisé".to_string())?
    };
    tx.send(WireMessage::FileOffer(offer.clone()))
        .map_err(|e| format!("Envoi offre échoué : {}", e))?;

    eprintln!(
        "[lan-chat] File offered: \"{}\" ({} bytes, sha256={}…)",
        offer.filename,
        offer.size,
        &offer.hash[..16.min(offer.hash.len())]
    );

    Ok(offer)
}

#[tauri::command]
pub fn download_file(
    app: AppHandle,
    file_id: String,
    sender_peer: String,
) -> Result<(), String> {
    let offer = received_offers_cell()
        .lock()
        .ok()
        .and_then(|g| g.get(&file_id).cloned())
        .ok_or_else(|| "Offre de fichier inconnue (expirée ?)".to_string())?;

    if offer.size > MAX_FILE_SIZE {
        return Err(format!(
            "Fichier trop gros ({} octets > max {} octets).",
            offer.size, MAX_FILE_SIZE
        ));
    }

    let peer = PeerId::from_str(&sender_peer).map_err(|e| format!("PeerId invalide : {}", e))?;

    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir : {}", e))?;
    let files_dir = app_data.join("files");
    fs::create_dir_all(&files_dir).map_err(|e| format!("mkdir files : {}", e))?;

    let final_path = dedupe_filename(&files_dir, &offer.filename);
    let temp_path = files_dir.join(format!(".{}.part", file_id));

    fs::File::create(&temp_path).map_err(|e| format!("create tempfile : {}", e))?;

    {
        let mut dl = active_downloads_cell()
            .lock()
            .map_err(|_| "active_downloads lock poisoned")?;
        if dl.contains_key(&file_id) {
            return Err("Téléchargement déjà en cours pour ce fichier.".into());
        }
        dl.insert(
            file_id.clone(),
            DownloadState {
                sender_peer: peer,
                size: offer.size,
                expected_hash: offer.hash.clone(),
                received_bytes: 0,
                temp_path,
                final_path,
            },
        );
    }

    let first_len = std::cmp::min(FILE_CHUNK_SIZE as u64, offer.size) as u32;
    let req = ChunkRequest {
        version: 1,
        file_id: file_id.clone(),
        offset: 0,
        length: first_len,
    };
    send_file_cmd(SwarmFileCmd::RequestChunk { peer, request: req });

    eprintln!(
        "[lan-chat] Download started: \"{}\" ({} bytes) from {}",
        offer.filename, offer.size, peer
    );
    Ok(())
}
