pub mod app_state;
pub mod crypto;
pub mod files;
pub mod history;
pub mod identity;
pub mod room;
pub mod swarm;
pub mod types;

use tauri::{async_runtime::JoinHandle, Emitter};
use tokio::sync::mpsc;

use crate::app_state::{
    clear_shared_state, file_cmd_tx_cell, manager_tx, node_state, publisher_tx_cell,
    set_node_status,
};
use crate::files::{download_file, offer_file, SwarmFileCmd};
use crate::history::{clear_history, get_history, send_message};
use crate::identity::load_or_create_identity;
use crate::room::{get_room_status, leave_room, load_room_config, reset_room, set_room_code};
use crate::swarm::run_swarm;
use crate::types::{ManagerCmd, NodeStatus, WireMessage};

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

            let keypair = match load_or_create_identity(&handle) {
                Ok(kp) => kp,
                Err(e) => {
                    let msg = format!("Échec du chargement de l'identité : {}", e);
                    eprintln!("[lan-chat] {}", msg);
                    set_node_status(NodeStatus::Error { message: msg.clone() });
                    let _ = handle.emit("node-error", msg);
                    return Ok(());
                }
            };

            let (tx, mut manager_rx) = mpsc::unbounded_channel::<ManagerCmd>();
            let _ = manager_tx().set(tx);

            tauri::async_runtime::spawn(async move {
                let mut current: Option<JoinHandle<()>> = None;

                match load_room_config(&handle) {
                    Some(cfg) => {
                        eprintln!(
                            "[lan-chat] Existing room config found: code=\"{}\" — auto-starting",
                            cfg.code
                        );
                        let _ = manager_tx().get().unwrap().send(ManagerCmd::StartRoom(cfg));
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

                            // Nouveaux canaux par run : les anciens Sender sont détachés.
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
                                    set_node_status(NodeStatus::Error { message: msg.clone() });
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
