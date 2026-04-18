use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use socket2::{Domain, Socket, Type};
use tauri::{AppHandle, Emitter};

const PORT: u16 = 9001;
const BROADCAST_ADDR: &str = "255.255.255.255";

// ─────────────────────────  État partagé du listener ─────────────────────────
// Permet au frontend d'interroger l'état courant à tout moment via invoke,
// ce qui évite le problème de race condition avec les events Tauri.

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ListenerStatus {
    Initializing,
    Ready { port: u16 },
    Error { message: String },
}

fn listener_state() -> &'static Mutex<ListenerStatus> {
    static STATE: OnceLock<Mutex<ListenerStatus>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ListenerStatus::Initializing))
}

fn set_listener_status(status: ListenerStatus) {
    if let Ok(mut guard) = listener_state().lock() {
        *guard = status;
    }
}

// ───────────────────────── Modèle de message ─────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    id: String,
    sender_name: String,
    sender_id: String,
    content: String,
    timestamp: u64,
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
fn get_listener_status() -> ListenerStatus {
    listener_state()
        .lock()
        .map(|g| g.clone())
        .unwrap_or(ListenerStatus::Initializing)
}

#[tauri::command]
fn send_message(message: ChatMessage) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;

    let payload = serde_json::to_string(&message).map_err(|e| e.to_string())?;
    let dest = format!("{}:{}", BROADCAST_ADDR, PORT);

    socket
        .send_to(payload.as_bytes(), dest)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ───────────────────────── Listener UDP ─────────────────────────

fn build_listener_socket() -> std::io::Result<UdpSocket> {
    // SO_REUSEADDR (et SO_REUSEPORT sur Unix) permet notamment de lancer
    // plusieurs instances de l'app sur la même machine pour les tests.
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, PORT).into();
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    #[cfg(not(target_os = "windows"))]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

fn spawn_listener(app: AppHandle) {
    thread::spawn(move || match build_listener_socket() {
        Ok(socket) => {
            set_listener_status(ListenerStatus::Ready { port: PORT });
            let _ = app.emit("listener-ready", PORT);

            let mut buf = [0u8; 4096];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((len, _addr)) => {
                        if let Ok(text) = std::str::from_utf8(&buf[..len]) {
                            if let Ok(msg) = serde_json::from_str::<ChatMessage>(text) {
                                let _ = app.emit("chat-message", msg);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[lan-chat] UDP recv error: {}", e);
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
        Err(e) => {
            let msg = format!("Impossible d'ouvrir le port {} : {}", PORT, e);
            eprintln!("[lan-chat] Listener init failed: {}", msg);
            set_listener_status(ListenerStatus::Error {
                message: msg.clone(),
            });
            let _ = app.emit("listener-error", msg);
        }
    });
}

// ───────────────────────── Entrée ─────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_hostname,
            get_listener_status,
            send_message
        ])
        .setup(|app| {
            spawn_listener(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
