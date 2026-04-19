use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
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
    gossipsub, identity, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, SwarmBuilder,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{io, select, sync::mpsc, sync::oneshot};

const APP_TAG: &str = "lan-chat-v1";

// ───────────────────────── Comportement libp2p ─────────────────────────

#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
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
#[derive(Serialize, Deserialize, Debug)]
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

// ───────────────────────── État partagé ─────────────────────────

fn node_state() -> &'static Mutex<NodeStatus> {
    static STATE: OnceLock<Mutex<NodeStatus>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NodeStatus::Initializing))
}

fn set_node_status(status: NodeStatus) {
    if let Ok(mut guard) = node_state().lock() {
        *guard = status;
    }
}

static PUBLISHER_TX: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
static ROOM_SETUP_TX: OnceLock<Mutex<Option<oneshot::Sender<RoomConfig>>>> = OnceLock::new();

fn take_room_setup_tx() -> Option<oneshot::Sender<RoomConfig>> {
    let cell = ROOM_SETUP_TX.get()?;
    let mut guard = cell.lock().ok()?;
    guard.take()
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

static HISTORY: OnceLock<Mutex<VecDeque<ChatMessage>>> = OnceLock::new();
static HISTORY_STORE: OnceLock<Arc<HistoryStore>> = OnceLock::new();

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
    // topic est "lan-chat-v1-XXXXXXXXXXXXXXXX" (16 hex) ; on garde la partie hash
    let hash_part = topic.strip_prefix(concat!("lan-chat-v1", "-")).unwrap_or(topic);
    app_data.join("history").join(format!("{}.bin", hash_part))
}

/// Ajoute un message à l'historique en mémoire et persiste (best-effort).
/// Silencieux si HISTORY ou HISTORY_STORE ne sont pas encore initialisés
/// (cas improbable : appelé uniquement après démarrage du swarm).
fn append_to_history(msg: ChatMessage) {
    let Some(hist_lock) = HISTORY.get() else { return };
    let Some(store) = HISTORY_STORE.get() else { return };

    let snapshot = {
        let mut hist = match hist_lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        hist.push_back(msg);
        while hist.len() > HISTORY_CAP {
            hist.pop_front();
        }
        hist.clone()
    };

    if let Err(e) = store.save(&snapshot) {
        eprintln!("[lan-chat] History save error: {}", e);
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

    // On consomme le Sender AVANT de persister pour éviter d'écraser
    // room.json avec un code qui ne sera jamais utilisé (2e appel = déjà actif).
    let tx = take_room_setup_tx().ok_or_else(|| {
        "Le salon est déjà actif. Utilise 'reset_room' puis relance l'application.".to_string()
    })?;

    let config = RoomConfig {
        version: 1,
        code: trimmed.to_string(),
    };
    save_room_config(&app, &config).map_err(|e| format!("Écriture de room.json : {}", e))?;

    // Transitionne le statut avant que le swarm soit prêt, pour que le frontend
    // ne repoll pas "awaitingRoom" et ne réaffiche pas l'overlay.
    set_node_status(NodeStatus::Initializing);

    let _ = tx.send(config);
    Ok(())
}

#[tauri::command]
fn reset_room(app: AppHandle) -> Result<(), String> {
    delete_room_config(&app).map_err(|e| format!("Suppression de room.json : {}", e))?;
    Ok(())
}

#[tauri::command]
fn send_message(message: ChatMessage) -> Result<(), String> {
    let tx = PUBLISHER_TX
        .get()
        .ok_or_else(|| "Publisher non initialisé".to_string())?;
    let json = serde_json::to_string(&message).map_err(|e| e.to_string())?;
    tx.send(json).map_err(|e| format!("Échec d'envoi : {}", e))?;
    // Archive immédiate de notre propre message (gossipsub ne nous le renvoie pas)
    append_to_history(message);
    Ok(())
}

#[tauri::command]
fn get_history() -> Vec<ChatMessage> {
    HISTORY
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.iter().cloned().collect()))
        .unwrap_or_default()
}

#[tauri::command]
fn clear_history() -> Result<(), String> {
    let store = HISTORY_STORE
        .get()
        .ok_or_else(|| "Store non initialisé".to_string())?;
    if let Some(hist_lock) = HISTORY.get() {
        if let Ok(mut g) = hist_lock.lock() {
            g.clear();
        }
    }
    store.clear()
}

// ───────────────────────── Boucle principale du swarm ─────────────────────────

async fn run_swarm(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<String>,
    keypair: identity::Keypair,
    room: RoomConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (topic_str, key_bytes) = derive_room(&room.code);
    let cipher = ChaCha20Poly1305::new_from_slice(&key_bytes)
        .map_err(|e| format!("Initialisation du chiffrement échouée : {}", e))?;

    // Historique chiffré : chemin par salon, clone du cipher pour (dé)chiffrement.
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
    let _ = HISTORY.set(Mutex::new(VecDeque::from(loaded_history)));
    let _ = HISTORY_STORE.set(history_store);
    // Le frontend récupère l'historique via invoke("get_history") dans applyStatus(ready).

    let topic_for_builder = topic_str.clone();
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(move |key| {
            // ID dérivé du hash du contenu → déduplication gossipsub
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

            Ok(ChatBehaviour { gossipsub, mdns })
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

    loop {
        select! {
            maybe_json = rx.recv() => {
                match maybe_json {
                    Some(plaintext) => {
                        // Chiffrement avant publication
                        let mut nonce_bytes = [0u8; 12];
                        rand::thread_rng().fill_bytes(&mut nonce_bytes);
                        let nonce = Nonce::from_slice(&nonce_bytes);

                        match cipher.encrypt(nonce, plaintext.as_bytes()) {
                            Ok(ct) => {
                                let envelope = EncryptedEnvelope {
                                    n: B64.encode(nonce_bytes),
                                    c: B64.encode(&ct),
                                };
                                match serde_json::to_vec(&envelope) {
                                    Ok(wire) => {
                                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), wire) {
                                            eprintln!("[lan-chat] Publish error: {}", e);
                                        }
                                    }
                                    Err(e) => eprintln!("[lan-chat] Envelope serialize error: {}", e),
                                }
                            }
                            Err(e) => eprintln!("[lan-chat] Encrypt error: {}", e),
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
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer, _addr) in list {
                            eprintln!("[lan-chat] mDNS expired: {}", peer);
                            swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer);
                        }
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message { message, .. })) => {
                        // Décryptage silencieux : une enveloppe illisible = salon différent ou tampering
                        let envelope = match serde_json::from_slice::<EncryptedEnvelope>(&message.data) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        let nonce_vec = match B64.decode(&envelope.n) {
                            Ok(v) if v.len() == 12 => v,
                            _ => continue,
                        };
                        let ct = match B64.decode(&envelope.c) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let nonce = Nonce::from_slice(&nonce_vec);
                        let plaintext = match cipher.decrypt(nonce, ct.as_slice()) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        if let Ok(text) = std::str::from_utf8(&plaintext) {
                            if let Ok(msg) = serde_json::from_str::<ChatMessage>(text) {
                                append_to_history(msg.clone());
                                let _ = app.emit("chat-message", msg);
                            }
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
            reset_room,
            send_message,
            get_history,
            clear_history
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            let (pub_tx, pub_rx) = mpsc::unbounded_channel::<String>();
            let _ = PUBLISHER_TX.set(pub_tx);

            let (room_tx, room_rx) = oneshot::channel::<RoomConfig>();
            let _ = ROOM_SETUP_TX.set(Mutex::new(Some(room_tx)));

            // Identité ed25519 persistante (chargée ou créée) — synchrone, avant le spawn
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

            tauri::async_runtime::spawn(async move {
                // Si un salon est déjà configuré, on l'utilise. Sinon, on attend que l'utilisateur
                // en fournisse un via set_room_code (via l'overlay du frontend).
                let room_config = match load_room_config(&handle) {
                    Some(cfg) => {
                        eprintln!(
                            "[lan-chat] Existing room config found: code=\"{}\" — skipping overlay",
                            cfg.code
                        );
                        // Le salon existe déjà → on consomme le Sender pour qu'il ne traîne pas.
                        let _ = take_room_setup_tx();
                        cfg
                    }
                    None => {
                        eprintln!("[lan-chat] No room config — awaiting user input via overlay");
                        set_node_status(NodeStatus::AwaitingRoom);
                        let _ = handle.emit("node-awaiting-room", ());
                        match room_rx.await {
                            Ok(cfg) => cfg,
                            Err(_) => {
                                eprintln!("[lan-chat] Room setup channel closed before use");
                                return;
                            }
                        }
                    }
                };

                if let Err(e) = run_swarm(handle.clone(), pub_rx, keypair, room_config).await {
                    let msg = format!("Erreur libp2p : {}", e);
                    eprintln!("[lan-chat] {}", msg);
                    set_node_status(NodeStatus::Error {
                        message: msg.clone(),
                    });
                    let _ = handle.emit("node-error", msg);
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
