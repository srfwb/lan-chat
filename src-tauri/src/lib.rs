use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use futures::stream::StreamExt;
use libp2p::{
    gossipsub, identity, mdns, noise, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, StreamProtocol, SwarmBuilder,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter, Manager};
use tokio::{io, select, sync::mpsc};

const APP_TAG: &str = "lan-chat-v1";

// ───────────────────────── Comportement libp2p ─────────────────────────

#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    history_sync: request_response::json::Behaviour<HistoryRequest, HistoryResponse>,
    file_transfer: request_response::json::Behaviour<ChunkRequest, ChunkResponse>,
}

const HISTORY_SYNC_PROTOCOL: &str = "/lan-chat/history-sync/1";
const FILE_TRANSFER_PROTOCOL: &str = "/lan-chat/file-transfer/1";

// Chunking config pour le transfert fichier
const FILE_CHUNK_SIZE: u32 = 256 * 1024; // 256 KiB
const MAX_FILE_SIZE: u64 = 500 * 1024 * 1024; // 500 MB

// ───────────────────────── Types wire pour la sync d'historique ─────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
struct HistoryRequest {
    version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct HistoryResponse {
    version: u32,
    messages: Vec<EncryptedEnvelope>,
}

// ───────────────────────── Types wire pour le transfert fichier ─────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ChunkRequest {
    version: u32,
    file_id: String,
    offset: u64,
    length: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ChunkResponse {
    Ok {
        file_id: String,
        data: EncryptedEnvelope,
    },
    NotFound,
    Error {
        message: String,
    },
}

// ───────────────────────── Modèles ─────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    id: String,
    sender_name: String,
    sender_id: String,
    content: String,
    timestamp: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct FileOffer {
    id: String,
    sender_name: String,
    sender_id: String,
    timestamp: u64,
    filename: String,
    size: u64,
    mime: String,
    hash: String,
}

/// Wrapper unifié pour tout ce qui transite via gossipsub : messages texte + annonces fichiers.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum WireMessage {
    Chat(ChatMessage),
    FileOffer(FileOffer),
}

/// Enveloppe chiffrée publiée sur gossipsub : jamais de clair sur le réseau.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct EncryptedEnvelope {
    /// Nonce 12 octets (base64).
    n: String,
    /// Ciphertext + tag AEAD (base64).
    c: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum NodeStatus {
    Initializing,
    AwaitingRoom,
    Ready { peer_id: String, room_name: String },
    Error { message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RoomConfig {
    version: u32,
    code: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomStatusResp {
    configured: bool,
    room_name: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NodeReadyPayload {
    peer_id: String,
    room_name: String,
}

/// Commandes envoyées à la task coordinatrice du swarm.
/// Permet le switch de salon à chaud : on abort le swarm courant et on en relance un nouveau.
enum ManagerCmd {
    StartRoom(RoomConfig),
    LeaveRoom,
}

// ───────────────────────── État partagé (cells réinitialisables) ─────────────────────────

fn node_state() -> &'static Mutex<NodeStatus> {
    static STATE: OnceLock<Mutex<NodeStatus>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NodeStatus::Initializing))
}

fn set_node_status(status: NodeStatus) {
    if let Ok(mut guard) = node_state().lock() {
        *guard = status;
    }
}

static HISTORY: OnceLock<Mutex<Option<VecDeque<ChatMessage>>>> = OnceLock::new();
static HISTORY_STORE: OnceLock<Mutex<Option<Arc<HistoryStore>>>> = OnceLock::new();
static PUBLISHER_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> = OnceLock::new();
static MANAGER_TX: OnceLock<mpsc::UnboundedSender<ManagerCmd>> = OnceLock::new();

// ─────────────── Fichiers : état partagé ───────────────

#[derive(Debug, Clone)]
struct ServedFile {
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone)]
struct DownloadState {
    sender_peer: PeerId,
    size: u64,
    expected_hash: String,
    received_bytes: u64,
    temp_path: PathBuf,
    final_path: PathBuf,
}

enum SwarmFileCmd {
    RequestChunk { peer: PeerId, request: ChunkRequest },
}

static SERVED_FILES: OnceLock<Mutex<HashMap<String, ServedFile>>> = OnceLock::new();
static RECEIVED_OFFERS: OnceLock<Mutex<HashMap<String, FileOffer>>> = OnceLock::new();
static ACTIVE_DOWNLOADS: OnceLock<Mutex<HashMap<String, DownloadState>>> = OnceLock::new();
static FILE_CMD_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<SwarmFileCmd>>>> = OnceLock::new();

fn history_cell() -> &'static Mutex<Option<VecDeque<ChatMessage>>> {
    HISTORY.get_or_init(|| Mutex::new(None))
}

fn history_store_cell() -> &'static Mutex<Option<Arc<HistoryStore>>> {
    HISTORY_STORE.get_or_init(|| Mutex::new(None))
}

fn publisher_tx_cell() -> &'static Mutex<Option<mpsc::UnboundedSender<WireMessage>>> {
    PUBLISHER_TX.get_or_init(|| Mutex::new(None))
}

fn served_files_cell() -> &'static Mutex<HashMap<String, ServedFile>> {
    SERVED_FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn received_offers_cell() -> &'static Mutex<HashMap<String, FileOffer>> {
    RECEIVED_OFFERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_downloads_cell() -> &'static Mutex<HashMap<String, DownloadState>> {
    ACTIVE_DOWNLOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_cmd_tx_cell() -> &'static Mutex<Option<mpsc::UnboundedSender<SwarmFileCmd>>> {
    FILE_CMD_TX.get_or_init(|| Mutex::new(None))
}

/// Vide toutes les ressources partagées liées à un salon (appelée lors d'un StartRoom ou LeaveRoom).
/// Le swarm précédent a été aborted juste avant ; son Drop ferme TCP/mDNS.
fn clear_shared_state() {
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
        // Best-effort cleanup des tempfiles en cours
        for (_, state) in g.iter() {
            let _ = fs::remove_file(&state.temp_path);
        }
        g.clear();
    }
}

// ───────────────────────── Identité persistante ─────────────────────────

const IDENTITY_FILE: &str = "identity.key";
const ROOM_FILE: &str = "room.json";

fn app_dir(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn identity_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(app_dir(app)?.join(IDENTITY_FILE))
}

fn room_config_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(app_dir(app)?.join(ROOM_FILE))
}

/// Charge la keypair ed25519 depuis `<app_data>/identity.key` si elle existe.
/// Sinon, en génère une nouvelle et la persiste. Encodage protobuf libp2p.
fn load_or_create_identity(
    app: &AppHandle,
) -> Result<identity::Keypair, Box<dyn std::error::Error>> {
    let path = identity_path(app)?;

    if path.exists() {
        let bytes = fs::read(&path)?;
        let keypair = identity::Keypair::from_protobuf_encoding(&bytes)?;
        eprintln!("[lan-chat] Identity loaded from {}", path.display());
        Ok(keypair)
    } else {
        let keypair = identity::Keypair::generate_ed25519();
        let bytes = keypair.to_protobuf_encoding()?;
        fs::write(&path, bytes)?;
        eprintln!("[lan-chat] Fresh identity generated at {}", path.display());
        Ok(keypair)
    }
}

// ───────────────────────── Configuration du salon ─────────────────────────

fn load_room_config(app: &AppHandle) -> Option<RoomConfig> {
    let path = room_config_path(app).ok()?;
    if !path.exists() {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice::<RoomConfig>(&bytes).ok()
}

fn save_room_config(app: &AppHandle, cfg: &RoomConfig) -> Result<(), Box<dyn std::error::Error>> {
    let path = room_config_path(app)?;
    let json = serde_json::to_vec_pretty(cfg)?;
    fs::write(&path, json)?;
    Ok(())
}

fn delete_room_config(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let path = room_config_path(app)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Dérive topic gossipsub et clé symétrique ChaCha20 à partir du code de salon.
/// - clé  = SHA256("lan-chat-v1|key|"   + code)
/// - topic = "lan-chat-v1-" + hex(SHA256("lan-chat-v1|topic|" + code))[..16]
fn derive_room(code: &str) -> (String, [u8; 32]) {
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

// ───────────────────────── Historique chiffré ─────────────────────────

const HISTORY_CAP: usize = 500;
const HISTORY_FILE_VERSION: u32 = 1;

struct HistoryStore {
    path: PathBuf,
    key: [u8; 32], // on garde la clé brute ; le cipher est reconstruit à la demande
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
    fn new(path: PathBuf, key: [u8; 32]) -> Self {
        Self { path, key }
    }

    fn cipher(&self) -> Result<ChaCha20Poly1305, String> {
        ChaCha20Poly1305::new_from_slice(&self.key).map_err(|e| e.to_string())
    }

    /// Charge l'historique depuis le disque (tolérant : renvoie Vec vide sur erreur).
    fn load(&self) -> Vec<ChatMessage> {
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

    /// Chiffre et écrit l'historique complet sur disque (atomic via rename).
    fn save(&self, messages: &VecDeque<ChatMessage>) -> Result<(), String> {
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

    fn clear(&self) -> Result<(), String> {
        if self.path.exists() {
            fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn history_path_for(app_data: &Path, topic: &str) -> PathBuf {
    let hash_part = topic.strip_prefix(concat!("lan-chat-v1", "-")).unwrap_or(topic);
    app_data.join("history").join(format!("{}.bin", hash_part))
}

/// Ajoute un message à l'historique en mémoire et persiste (best-effort).
/// Retourne true si le message a été ajouté (nouveau) ou false s'il était déjà présent
/// (dédup par ID) ou si les stores ne sont pas initialisés.
fn append_to_history_if_new(msg: ChatMessage) -> bool {
    let snapshot = {
        let mut guard = match history_cell().lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let hist = match guard.as_mut() {
            Some(h) => h,
            None => return false,
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

/// Alias compat pour l'appel dans `send_message` : on jette le bool.
fn append_to_history(msg: ChatMessage) {
    let _ = append_to_history_if_new(msg);
}

// ───────────────────────── Chiffrement helpers (factorisé) ─────────────────────────

/// Chiffre un blob de bytes quelconque avec la clé du salon, nonce aléatoire par appel.
fn encrypt_bytes(cipher: &ChaCha20Poly1305, data: &[u8]) -> Option<EncryptedEnvelope> {
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, data).ok()?;
    Some(EncryptedEnvelope {
        n: B64.encode(nonce_bytes),
        c: B64.encode(&ct),
    })
}

/// Déchiffre une enveloppe en blob de bytes. Silencieux sur toute erreur (mauvaise clé, corruption).
fn decrypt_bytes(cipher: &ChaCha20Poly1305, env: &EncryptedEnvelope) -> Option<Vec<u8>> {
    let nonce_bytes = B64.decode(&env.n).ok()?;
    if nonce_bytes.len() != 12 {
        return None;
    }
    let ct = B64.decode(&env.c).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    cipher.decrypt(nonce, ct.as_slice()).ok()
}

/// Helpers typés : wire messages gossipsub (chat + file offers).
fn encrypt_wire_message(cipher: &ChaCha20Poly1305, msg: &WireMessage) -> Option<EncryptedEnvelope> {
    let bytes = serde_json::to_vec(msg).ok()?;
    encrypt_bytes(cipher, &bytes)
}

fn decrypt_wire_message(cipher: &ChaCha20Poly1305, env: &EncryptedEnvelope) -> Option<WireMessage> {
    let bytes = decrypt_bytes(cipher, env)?;
    serde_json::from_slice::<WireMessage>(&bytes).ok()
}

/// Helpers typés : ChatMessage standalone (utilisé par la history sync, qui échange
/// directement des ChatMessages indépendants de l'enum WireMessage).
fn encrypt_chat_message(cipher: &ChaCha20Poly1305, msg: &ChatMessage) -> Option<EncryptedEnvelope> {
    let bytes = serde_json::to_vec(msg).ok()?;
    encrypt_bytes(cipher, &bytes)
}

fn decrypt_chat_message(cipher: &ChaCha20Poly1305, env: &EncryptedEnvelope) -> Option<ChatMessage> {
    let bytes = decrypt_bytes(cipher, env)?;
    serde_json::from_slice::<ChatMessage>(&bytes).ok()
}

// ───────────────────────── Sync d'historique P2P ─────────────────────────

fn build_history_response(cipher: &ChaCha20Poly1305) -> HistoryResponse {
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

fn ingest_history_response(
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
                    // Event dédié : le frontend insère en ordre chronologique avant le séparateur.
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

// ───────────────────────── Fichiers : helpers ─────────────────────────

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hash SHA256 en streaming depuis un fichier (buffer 256K) — pas de chargement complet en RAM.
fn compute_sha256_file(path: &Path) -> Result<String, String> {
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

/// Détermine un chemin de destination qui ne collision pas : si `foo.pdf` existe,
/// retourne `foo (1).pdf`, puis `foo (2).pdf`, etc.
fn dedupe_filename(dir: &Path, filename: &str) -> PathBuf {
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
    dir.join(filename) // abandonne, renvoie le nom de base même si existe
}

/// Sert un chunk en lisant la plage demandée depuis le disque, puis en la chiffrant.
fn serve_chunk(cipher: &ChaCha20Poly1305, req: &ChunkRequest) -> ChunkResponse {
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

/// Intégre un chunk reçu : decrypt → write to .part → progress → next chunk ou finalize.
fn ingest_chunk(
    cipher: &ChaCha20Poly1305,
    file_id: &str,
    envelope: EncryptedEnvelope,
    app: &AppHandle,
) {
    // 1. Décrypter
    let Some(plaintext) = decrypt_bytes(cipher, &envelope) else {
        emit_file_error(app, file_id, "decrypt failed");
        abort_download(file_id);
        return;
    };

    // 2. Écrire le chunk, maj de l'état, calcul de la suite. Tout sous lock, court.
    let (received, total, temp_path, sender_peer, completed) = {
        let mut dl = match active_downloads_cell().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(state) = dl.get_mut(file_id) else {
            return; // téléchargement annulé (leave_room, etc.)
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
            drop(dl); // release lock avant l'emit
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
        // Demande du chunk suivant
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
    // Vérification SHA256 en streaming
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

    // On retire l'entrée
    if let Ok(mut g) = active_downloads_cell().lock() {
        g.remove(file_id);
    }
}

fn emit_file_error(app: &AppHandle, file_id: &str, msg: &str) {
    eprintln!("[lan-chat] File error ({}): {}", file_id, msg);
    let _ = app.emit(
        "file-error",
        serde_json::json!({
            "fileId": file_id,
            "message": msg,
        }),
    );
}

fn abort_download(file_id: &str) {
    if let Ok(mut g) = active_downloads_cell().lock() {
        g.remove(file_id);
    }
}

fn send_file_cmd(cmd: SwarmFileCmd) {
    let tx = file_cmd_tx_cell().lock().ok().and_then(|g| g.as_ref().cloned());
    let Some(tx) = tx else {
        eprintln!("[lan-chat] File command channel not initialized");
        return;
    };
    if let Err(e) = tx.send(cmd) {
        eprintln!("[lan-chat] Failed to send file cmd: {}", e);
    }
}

// ───────────────────────── Commandes Tauri ─────────────────────────

#[tauri::command]
fn get_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "Inconnu".into())
}

#[tauri::command]
fn get_node_status() -> NodeStatus {
    node_state()
        .lock()
        .map(|g| g.clone())
        .unwrap_or(NodeStatus::Initializing)
}

#[tauri::command]
fn get_room_status(app: AppHandle) -> RoomStatusResp {
    let cfg = load_room_config(&app);
    RoomStatusResp {
        configured: cfg.is_some(),
        room_name: cfg.map(|c| c.code),
    }
}

#[tauri::command]
fn set_room_code(app: AppHandle, code: String) -> Result<(), String> {
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

    MANAGER_TX
        .get()
        .ok_or_else(|| "Manager non initialisé".to_string())?
        .send(ManagerCmd::StartRoom(config))
        .map_err(|e| format!("Canal manager fermé : {}", e))?;

    // Le manager transitionne vers Initializing puis Ready ; on ne touche pas
    // au status ici pour éviter de clobber un Ready déjà posé (même race que le
    // fix frontend da4de6b).
    Ok(())
}

#[tauri::command]
fn leave_room(app: AppHandle) -> Result<(), String> {
    delete_room_config(&app).map_err(|e| format!("Suppression de room.json : {}", e))?;
    MANAGER_TX
        .get()
        .ok_or_else(|| "Manager non initialisé".to_string())?
        .send(ManagerCmd::LeaveRoom)
        .map_err(|e| format!("Canal manager fermé : {}", e))?;
    Ok(())
}

/// Legacy alias : le frontend d'anciennes versions appelait reset_room.
/// Dans la nouvelle version, c'est équivalent à leave_room.
#[tauri::command]
fn reset_room(app: AppHandle) -> Result<(), String> {
    leave_room(app)
}

#[tauri::command]
fn send_message(message: ChatMessage) -> Result<(), String> {
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
    // Archive immédiate de notre propre message (gossipsub ne nous le renvoie pas)
    append_to_history(message);
    Ok(())
}

#[tauri::command]
fn offer_file(
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

    // On a besoin du PeerId local pour signer l'offer. On le lit depuis node_state.
    let sender_id = match node_state().lock().ok().map(|g| g.clone()) {
        Some(NodeStatus::Ready { peer_id, .. }) => peer_id,
        _ => return Err("Nœud P2P pas encore prêt.".into()),
    };

    let mime = mime_guess::from_path(&src)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    // Hash streaming (pas de chargement complet en RAM)
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

    // Enregistre la ServedFile localement avant de publier l'annonce.
    served_files_cell().lock().map_err(|_| "lock poisoned")?.insert(
        file_id.clone(),
        ServedFile {
            path: src.clone(),
            size,
        },
    );

    // Publie l'annonce via gossipsub (wrap en WireMessage::FileOffer)
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
fn download_file(app: AppHandle, file_id: String, sender_peer: String) -> Result<(), String> {
    // Retrouve l'offer
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

    // Truncate (crée si absent, vide si présent)
    fs::File::create(&temp_path).map_err(|e| format!("create tempfile : {}", e))?;

    // Déjà un download en cours pour ce file_id ? On skip proprement.
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

    // Premier chunk
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

#[tauri::command]
fn get_history() -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = history_cell()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|h| h.iter().cloned().collect()))
        .unwrap_or_default();
    // Tri par timestamp : la deque peut être désordonnée après des syncs P2P
    // (append_to_history_if_new fait un push_back, pas une insertion triée).
    messages.sort_by_key(|m| m.timestamp);
    messages
}

#[tauri::command]
fn clear_history() -> Result<(), String> {
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

// ───────────────────────── Boucle principale du swarm ─────────────────────────

async fn run_swarm(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<WireMessage>,
    mut file_cmd_rx: mpsc::UnboundedReceiver<SwarmFileCmd>,
    keypair: identity::Keypair,
    room: RoomConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (topic_str, key_bytes) = derive_room(&room.code);
    let cipher = ChaCha20Poly1305::new_from_slice(&key_bytes)
        .map_err(|e| format!("Initialisation du chiffrement échouée : {}", e))?;

    // Historique chiffré : chemin par salon, clé brute (cipher reconstruit à la demande).
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {}", e))?;
    let history_store = Arc::new(HistoryStore::new(
        history_path_for(&app_data, &topic_str),
        key_bytes,
    ));
    let loaded_history = history_store.load();
    eprintln!(
        "[lan-chat] History loaded: {} message(s) from {}",
        loaded_history.len(),
        history_store.path.display()
    );

    // Publie dans les cells (overwrite si déjà set par un run précédent).
    if let Ok(mut g) = history_cell().lock() {
        *g = Some(VecDeque::from(loaded_history));
    }
    if let Ok(mut g) = history_store_cell().lock() {
        *g = Some(history_store);
    }

    let topic_for_builder = topic_str.clone();
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(move |key| {
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(1))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

            let mut gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

            let topic = gossipsub::IdentTopic::new(topic_for_builder.as_str());
            gossipsub
                .subscribe(&topic)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let mdns = mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                key.public().to_peer_id(),
            )?;

            let history_sync = request_response::json::Behaviour::<HistoryRequest, HistoryResponse>::new(
                [(
                    StreamProtocol::new(HISTORY_SYNC_PROTOCOL),
                    request_response::ProtocolSupport::Full,
                )],
                request_response::Config::default(),
            );

            let file_transfer = request_response::json::Behaviour::<ChunkRequest, ChunkResponse>::new(
                [(
                    StreamProtocol::new(FILE_TRANSFER_PROTOCOL),
                    request_response::ProtocolSupport::Full,
                )],
                request_response::Config::default()
                    .with_request_timeout(Duration::from_secs(30)),
            );

            Ok(ChatBehaviour {
                gossipsub,
                mdns,
                history_sync,
                file_transfer,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    let peer_id = *swarm.local_peer_id();
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    set_node_status(NodeStatus::Ready {
        peer_id: peer_id.to_string(),
        room_name: room.code.clone(),
    });
    let _ = app.emit(
        "node-ready",
        NodeReadyPayload {
            peer_id: peer_id.to_string(),
            room_name: room.code.clone(),
        },
    );

    let topic = gossipsub::IdentTopic::new(topic_str.as_str());

    // Tracké par run_swarm : un seul sync par pair par session.
    // Reset implicite à chaque restart du swarm (cette variable est locale).
    let mut synced_peers: HashSet<PeerId> = HashSet::new();

    loop {
        select! {
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    Some(wire_message) => {
                        if let Some(envelope) = encrypt_wire_message(&cipher, &wire_message) {
                            match serde_json::to_vec(&envelope) {
                                Ok(bytes) => {
                                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), bytes) {
                                        eprintln!("[lan-chat] Publish error: {}", e);
                                    }
                                }
                                Err(e) => eprintln!("[lan-chat] Envelope serialize error: {}", e),
                            }
                        } else {
                            eprintln!("[lan-chat] Failed to encrypt outgoing message");
                        }
                    }
                    None => break,
                }
            }
            maybe_cmd = file_cmd_rx.recv() => {
                match maybe_cmd {
                    Some(SwarmFileCmd::RequestChunk { peer, request }) => {
                        swarm.behaviour_mut().file_transfer.send_request(&peer, request);
                    }
                    None => break,
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (peer, _addr) in list {
                            eprintln!("[lan-chat] mDNS discovered: {}", peer);
                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                            // Sync d'historique : une seule fois par pair par session.
                            if synced_peers.insert(peer) {
                                swarm
                                    .behaviour_mut()
                                    .history_sync
                                    .send_request(&peer, HistoryRequest { version: 1 });
                                let _ = app.emit("history-sync-start", peer.to_string());
                            }
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer, _addr) in list {
                            eprintln!("[lan-chat] mDNS expired: {}", peer);
                            swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer);
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message { message, .. })) => {
                        let envelope = match serde_json::from_slice::<EncryptedEnvelope>(&message.data) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        match decrypt_wire_message(&cipher, &envelope) {
                            Some(WireMessage::Chat(msg)) => {
                                if append_to_history_if_new(msg.clone()) {
                                    let _ = app.emit("chat-message", msg);
                                }
                            }
                            Some(WireMessage::FileOffer(offer)) => {
                                // Stocker l'offer pour que download_file puisse la retrouver plus tard
                                if let Ok(mut g) = received_offers_cell().lock() {
                                    g.insert(offer.id.clone(), offer.clone());
                                }
                                eprintln!(
                                    "[lan-chat] File offer received: \"{}\" ({} bytes) from {}",
                                    offer.filename, offer.size, offer.sender_id
                                );
                                let _ = app.emit("file-offer", offer);
                            }
                            None => {
                                // Décryptage échoué — silencieux, salon voisin ou tampering
                            }
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::FileTransfer(ev)) => {
                        match ev {
                            request_response::Event::Message { peer, message, .. } => {
                                match message {
                                    request_response::Message::Request { request, channel, .. } => {
                                        let response = serve_chunk(&cipher, &request);
                                        if let ChunkResponse::Ok { ref file_id, .. } = response {
                                            eprintln!(
                                                "[lan-chat] Served chunk of {} @ offset {} ({} bytes) to {}",
                                                file_id, request.offset, request.length, peer
                                            );
                                        }
                                        let _ = swarm
                                            .behaviour_mut()
                                            .file_transfer
                                            .send_response(channel, response);
                                    }
                                    request_response::Message::Response { response, .. } => {
                                        match response {
                                            ChunkResponse::Ok { file_id, data } => {
                                                ingest_chunk(&cipher, &file_id, data, &app);
                                            }
                                            ChunkResponse::NotFound => {
                                                // On ne peut pas identifier le file_id ici (absent de la variante).
                                                // On logge seulement ; l'UI pourra détecter via un timeout optionnel (follow-up).
                                                eprintln!("[lan-chat] Chunk response: NotFound from {}", peer);
                                            }
                                            ChunkResponse::Error { message } => {
                                                eprintln!("[lan-chat] Chunk response error from {}: {}", peer, message);
                                            }
                                        }
                                    }
                                }
                            }
                            request_response::Event::OutboundFailure { peer, error, .. } => {
                                eprintln!("[lan-chat] File transfer outbound failure to {}: {}", peer, error);
                            }
                            request_response::Event::InboundFailure { peer, error, .. } => {
                                eprintln!("[lan-chat] File transfer inbound failure from {}: {}", peer, error);
                            }
                            request_response::Event::ResponseSent { .. } => {}
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::HistorySync(ev)) => {
                        match ev {
                            request_response::Event::Message { peer, message, .. } => {
                                match message {
                                    request_response::Message::Request { request, channel, .. } => {
                                        if request.version == 1 {
                                            let response = build_history_response(&cipher);
                                            eprintln!(
                                                "[lan-chat] Sending {} msg(s) in history-sync response to {}",
                                                response.messages.len(),
                                                peer
                                            );
                                            let _ = swarm
                                                .behaviour_mut()
                                                .history_sync
                                                .send_response(channel, response);
                                        } else {
                                            eprintln!(
                                                "[lan-chat] Ignoring history-sync request from {} (unsupported version {})",
                                                peer, request.version
                                            );
                                            // channel est drop → le pair recevra ResponseOmission
                                        }
                                    }
                                    request_response::Message::Response { response, .. } => {
                                        eprintln!(
                                            "[lan-chat] Received {} msg(s) in history-sync response from {}",
                                            response.messages.len(),
                                            peer
                                        );
                                        ingest_history_response(&cipher, response, &app);
                                        let _ = app.emit("history-sync-end", peer.to_string());
                                    }
                                }
                            }
                            request_response::Event::OutboundFailure { peer, error, .. } => {
                                eprintln!("[lan-chat] History sync outbound failure to {}: {}", peer, error);
                                let _ = app.emit("history-sync-end", peer.to_string());
                            }
                            request_response::Event::InboundFailure { peer, error, .. } => {
                                eprintln!("[lan-chat] History sync inbound failure from {}: {}", peer, error);
                            }
                            request_response::Event::ResponseSent { .. } => {}
                        }
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        eprintln!("[lan-chat] Listening on {}", address);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

// ───────────────────────── Entrée ─────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            get_hostname,
            get_node_status,
            get_room_status,
            set_room_code,
            leave_room,
            reset_room,
            send_message,
            offer_file,
            download_file,
            get_history,
            clear_history
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Identité ed25519 persistante, chargée avant de spawn le manager.
            let keypair = match load_or_create_identity(&handle) {
                Ok(kp) => kp,
                Err(e) => {
                    let msg = format!("Échec du chargement de l'identité : {}", e);
                    eprintln!("[lan-chat] {}", msg);
                    set_node_status(NodeStatus::Error {
                        message: msg.clone(),
                    });
                    let _ = handle.emit("node-error", msg);
                    return Ok(());
                }
            };

            // Canal multi-commande pour orchestrer le cycle de vie du swarm.
            let (manager_tx, mut manager_rx) = mpsc::unbounded_channel::<ManagerCmd>();
            let _ = MANAGER_TX.set(manager_tx);

            tauri::async_runtime::spawn(async move {
                let mut current: Option<JoinHandle<()>> = None;

                // Auto-start si un room.json existe. Sinon on passe en AwaitingRoom.
                match load_room_config(&handle) {
                    Some(cfg) => {
                        eprintln!(
                            "[lan-chat] Existing room config found: code=\"{}\" — auto-starting",
                            cfg.code
                        );
                        let _ = MANAGER_TX.get().unwrap().send(ManagerCmd::StartRoom(cfg));
                    }
                    None => {
                        eprintln!("[lan-chat] No room config — awaiting user input via overlay");
                        set_node_status(NodeStatus::AwaitingRoom);
                        let _ = handle.emit("node-awaiting-room", ());
                    }
                }

                while let Some(cmd) = manager_rx.recv().await {
                    match cmd {
                        ManagerCmd::StartRoom(config) => {
                            eprintln!("[lan-chat] Manager: StartRoom(\"{}\")", config.code);
                            if let Some(h) = current.take() {
                                h.abort();
                            }
                            clear_shared_state();
                            set_node_status(NodeStatus::Initializing);

                            // Nouveaux canaux par run (les anciens Sender sont détachés).
                            let (pub_tx, pub_rx) = mpsc::unbounded_channel::<WireMessage>();
                            if let Ok(mut g) = publisher_tx_cell().lock() {
                                *g = Some(pub_tx);
                            }
                            let (file_cmd_tx, file_cmd_rx) =
                                mpsc::unbounded_channel::<SwarmFileCmd>();
                            if let Ok(mut g) = file_cmd_tx_cell().lock() {
                                *g = Some(file_cmd_tx);
                            }

                            let h_app = handle.clone();
                            let kp = keypair.clone();
                            current = Some(tauri::async_runtime::spawn(async move {
                                if let Err(e) =
                                    run_swarm(h_app.clone(), pub_rx, file_cmd_rx, kp, config).await
                                {
                                    let msg = format!("Erreur libp2p : {}", e);
                                    eprintln!("[lan-chat] {}", msg);
                                    set_node_status(NodeStatus::Error {
                                        message: msg.clone(),
                                    });
                                    let _ = h_app.emit("node-error", msg);
                                }
                            }));
                        }
                        ManagerCmd::LeaveRoom => {
                            eprintln!("[lan-chat] Manager: LeaveRoom");
                            if let Some(h) = current.take() {
                                h.abort();
                            }
                            clear_shared_state();
                            set_node_status(NodeStatus::AwaitingRoom);
                            let _ = handle.emit("node-awaiting-room", ());
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
