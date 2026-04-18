use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::stream::StreamExt;
use libp2p::{
    gossipsub, identity, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{io, select, sync::mpsc};

const TOPIC: &str = "lan-chat-v1";

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

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum NodeStatus {
    Initializing,
    Ready { peer_id: String },
    Error { message: String },
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

// ───────────────────────── Identité persistante ─────────────────────────

const IDENTITY_FILE: &str = "identity.key";

fn identity_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir.join(IDENTITY_FILE))
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
fn send_message(message: ChatMessage) -> Result<(), String> {
    let tx = PUBLISHER_TX
        .get()
        .ok_or_else(|| "Publisher non initialisé".to_string())?;
    let json = serde_json::to_string(&message).map_err(|e| e.to_string())?;
    tx.send(json).map_err(|e| format!("Échec d'envoi : {}", e))?;
    Ok(())
}

// ───────────────────────── Boucle principale du swarm ─────────────────────────

async fn run_swarm(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<String>,
    keypair: identity::Keypair,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
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

            let topic = gossipsub::IdentTopic::new(TOPIC);
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
    });
    let _ = app.emit("node-ready", peer_id.to_string());

    let topic = gossipsub::IdentTopic::new(TOPIC);

    loop {
        select! {
            maybe_json = rx.recv() => {
                match maybe_json {
                    Some(json) => {
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), json.as_bytes()) {
                            eprintln!("[lan-chat] Publish error: {}", e);
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
                        if let Ok(text) = std::str::from_utf8(&message.data) {
                            if let Ok(msg) = serde_json::from_str::<ChatMessage>(text) {
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
            send_message
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            let _ = PUBLISHER_TX.set(tx);

            // Identité ed25519 persistante (chargée ou créée)
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
                if let Err(e) = run_swarm(handle.clone(), rx, keypair).await {
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
