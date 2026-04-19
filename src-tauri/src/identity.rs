use std::fs;
use std::path::PathBuf;

use libp2p::identity;
use tauri::{AppHandle, Manager};

const IDENTITY_FILE: &str = "identity.key";

pub fn app_dir(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn identity_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(app_dir(app)?.join(IDENTITY_FILE))
}

/// Charge la keypair ed25519, ou en génère une nouvelle et la persiste (protobuf libp2p).
pub fn load_or_create_identity(
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
