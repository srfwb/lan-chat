use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
}

const HISTORY_SYNC_PROTOCOL: &str = "/lan-chat/history-sync/1";

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
static PUBLISHER_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<ChatMessage>>>> = OnceLock::new();
static MANAGER_TX: OnceLock<mpsc::UnboundedSender<ManagerCmd>> = OnceLock::new();

fn history_cell() -> &'static Mutex<Option<VecDeque<ChatMessage>>> {
    HISTORY.get_or_init(|| Mutex::new(None))
}

fn history_store_cell() -> &'static Mutex<Option<Arc<HistoryStore>>> {
    HISTORY_STORE.get_or_init(|| Mutex::new(None))
}

fn publisher_tx_cell() -> &'static Mutex<Option<mpsc::UnboundedSender<ChatMessage>>> {
    PUBLISHER_TX.get_or_init(|| Mutex::new(None))
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

fn encrypt_chat_message(cipher: &ChaCha20Poly1305, msg: &ChatMessage) -> Option<EncryptedEnvelope> {
    let json = serde_json::to_vec(msg).ok()?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, json.as_slice()).ok()?;
    Some(EncryptedEnvelope {
        n: B64.encode(nonce_bytes),
        c: B64.encode(&ct),
    })
}

fn decrypt_envelope(cipher: &ChaCha20Poly1305, env: &EncryptedEnvelope) -> Option<ChatMessage> {
    let nonce_bytes = B64.decode(&env.n).ok()?;
    if nonce_bytes.len() != 12 {
        return None;
    }
    let ct = B64.decode(&env.c).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ct.as_slice()).ok()?;
    serde_json::from_slice::<ChatMessage>(&plaintext).ok()
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
        match decrypt_envelope(cipher, &envelope) {
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
    tx.send(message.clone())
        .map_err(|e| format!("Échec d'envoi : {}", e))?;
    // Archive immédiate de notre propre message (gossipsub ne nous le renvoie pas)
    append_to_history(message);
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
    mut rx: mpsc::UnboundedReceiver<ChatMessage>,
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

            Ok(ChatBehaviour {
                gossipsub,
                mdns,
                history_sync,
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
                    Some(message) => {
                        if let Some(envelope) = encrypt_chat_message(&cipher, &message) {
                            match serde_json::to_vec(&envelope) {
                                Ok(wire) => {
                                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), wire) {
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
                        if let Some(msg) = decrypt_envelope(&cipher, &envelope) {
                            if append_to_history_if_new(msg.clone()) {
                                let _ = app.emit("chat-message", msg);
                            }
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
        .invoke_handler(tauri::generate_handler![
            get_hostname,
            get_node_status,
            get_room_status,
            set_room_code,
            leave_room,
            reset_room,
            send_message,
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

                            // Nouveau canal publisher par run (les anciens Sender sont détachés).
                            let (pub_tx, pub_rx) = mpsc::unbounded_channel::<ChatMessage>();
                            if let Ok(mut g) = publisher_tx_cell().lock() {
                                *g = Some(pub_tx);
                            }

                            let h_app = handle.clone();
                            let kp = keypair.clone();
                            current = Some(tauri::async_runtime::spawn(async move {
                                if let Err(e) = run_swarm(h_app.clone(), pub_rx, kp, config).await {
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
