use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use chacha20poly1305::ChaCha20Poly1305;
use futures::stream::StreamExt;
use libp2p::{
    gossipsub, identity, mdns, noise, request_response,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, StreamProtocol, SwarmBuilder,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{io, select, sync::mpsc};

use crate::app_state::{
    history_cell, history_store_cell, received_offers_cell, set_node_status,
};
use crate::crypto::{cipher_from_key, decrypt_wire_message, encrypt_wire_message};
use crate::files::{
    ingest_chunk, serve_chunk, ChunkRequest, ChunkResponse, SwarmFileCmd,
};
use crate::history::{
    append_to_history_if_new, build_history_response, history_path_for, ingest_history_response,
    HistoryRequest, HistoryResponse, HistoryStore,
};
use crate::room::derive_room;
use crate::types::{
    EncryptedEnvelope, NodeReadyPayload, NodeStatus, RoomConfig, WireMessage,
};

const HISTORY_SYNC_PROTOCOL: &str = "/lan-chat/history-sync/1";
const FILE_TRANSFER_PROTOCOL: &str = "/lan-chat/file-transfer/1";

#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    history_sync: request_response::json::Behaviour<HistoryRequest, HistoryResponse>,
    file_transfer: request_response::json::Behaviour<ChunkRequest, ChunkResponse>,
}

pub async fn run_swarm(
    app: AppHandle,
    mut rx: mpsc::UnboundedReceiver<WireMessage>,
    mut file_cmd_rx: mpsc::UnboundedReceiver<SwarmFileCmd>,
    keypair: identity::Keypair,
    room: RoomConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (topic_str, key_bytes) = derive_room(&room.code);
    let cipher = cipher_from_key(&key_bytes)
        .map_err(|e| format!("Initialisation du chiffrement échouée : {}", e))?;

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

            let history_sync =
                request_response::json::Behaviour::<HistoryRequest, HistoryResponse>::new(
                    [(
                        StreamProtocol::new(HISTORY_SYNC_PROTOCOL),
                        request_response::ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                );

            let file_transfer =
                request_response::json::Behaviour::<ChunkRequest, ChunkResponse>::new(
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

    // Un seul sync d'historique par pair par session (reset implicite au respawn du swarm).
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
                handle_swarm_event(event, &mut swarm, &cipher, &app, &mut synced_peers);
            }
        }
    }

    Ok(())
}

fn handle_swarm_event(
    event: SwarmEvent<ChatBehaviourEvent>,
    swarm: &mut libp2p::Swarm<ChatBehaviour>,
    cipher: &ChaCha20Poly1305,
    app: &AppHandle,
    synced_peers: &mut HashSet<PeerId>,
) {
    match event {
        SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer, _addr) in list {
                eprintln!("[lan-chat] mDNS discovered: {}", peer);
                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
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
        SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            message,
            ..
        })) => {
            let envelope = match serde_json::from_slice::<EncryptedEnvelope>(&message.data) {
                Ok(e) => e,
                Err(_) => return,
            };
            match decrypt_wire_message(cipher, &envelope) {
                Some(WireMessage::Chat(msg)) => {
                    if append_to_history_if_new(msg.clone()) {
                        let _ = app.emit("chat-message", msg);
                    }
                }
                Some(WireMessage::FileOffer(offer)) => {
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
                    // Décryptage échoué : salon voisin ou tampering — silencieux.
                }
            }
        }
        SwarmEvent::Behaviour(ChatBehaviourEvent::FileTransfer(ev)) => match ev {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let response = serve_chunk(cipher, &request);
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
                request_response::Message::Response { response, .. } => match response {
                    ChunkResponse::Ok { file_id, data } => {
                        ingest_chunk(cipher, &file_id, data, app);
                    }
                    ChunkResponse::NotFound => {
                        eprintln!("[lan-chat] Chunk response: NotFound from {}", peer);
                    }
                    ChunkResponse::Error { message } => {
                        eprintln!(
                            "[lan-chat] Chunk response error from {}: {}",
                            peer, message
                        );
                    }
                },
            },
            request_response::Event::OutboundFailure { peer, error, .. } => {
                eprintln!(
                    "[lan-chat] File transfer outbound failure to {}: {}",
                    peer, error
                );
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                eprintln!(
                    "[lan-chat] File transfer inbound failure from {}: {}",
                    peer, error
                );
            }
            request_response::Event::ResponseSent { .. } => {}
        },
        SwarmEvent::Behaviour(ChatBehaviourEvent::HistorySync(ev)) => match ev {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    if request.version == 1 {
                        let response = build_history_response(cipher);
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
                        // channel drop → ResponseOmission côté pair
                    }
                }
                request_response::Message::Response { response, .. } => {
                    eprintln!(
                        "[lan-chat] Received {} msg(s) in history-sync response from {}",
                        response.messages.len(),
                        peer
                    );
                    ingest_history_response(cipher, response, app);
                    let _ = app.emit("history-sync-end", peer.to_string());
                }
            },
            request_response::Event::OutboundFailure { peer, error, .. } => {
                eprintln!(
                    "[lan-chat] History sync outbound failure to {}: {}",
                    peer, error
                );
                let _ = app.emit("history-sync-end", peer.to_string());
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                eprintln!(
                    "[lan-chat] History sync inbound failure from {}: {}",
                    peer, error
                );
            }
            request_response::Event::ResponseSent { .. } => {}
        },
        SwarmEvent::NewListenAddr { address, .. } => {
            eprintln!("[lan-chat] Listening on {}", address);
        }
        _ => {}
    }
}
