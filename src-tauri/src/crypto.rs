use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::RngCore;

use crate::types::{ChatMessage, EncryptedEnvelope, WireMessage};

pub fn cipher_from_key(key: &[u8]) -> Result<ChaCha20Poly1305, String> {
    ChaCha20Poly1305::new_from_slice(key).map_err(|e| e.to_string())
}

pub fn encrypt_bytes(cipher: &ChaCha20Poly1305, data: &[u8]) -> Option<EncryptedEnvelope> {
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, data).ok()?;
    Some(EncryptedEnvelope {
        n: B64.encode(nonce_bytes),
        c: B64.encode(&ct),
    })
}

pub fn decrypt_bytes(cipher: &ChaCha20Poly1305, env: &EncryptedEnvelope) -> Option<Vec<u8>> {
    let nonce_bytes = B64.decode(&env.n).ok()?;
    if nonce_bytes.len() != 12 {
        return None;
    }
    let ct = B64.decode(&env.c).ok()?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    cipher.decrypt(nonce, ct.as_slice()).ok()
}

pub fn encrypt_wire_message(
    cipher: &ChaCha20Poly1305,
    msg: &WireMessage,
) -> Option<EncryptedEnvelope> {
    let bytes = serde_json::to_vec(msg).ok()?;
    encrypt_bytes(cipher, &bytes)
}

pub fn decrypt_wire_message(
    cipher: &ChaCha20Poly1305,
    env: &EncryptedEnvelope,
) -> Option<WireMessage> {
    let bytes = decrypt_bytes(cipher, env)?;
    serde_json::from_slice::<WireMessage>(&bytes).ok()
}

/// La history sync échange des ChatMessages indépendants du wrapper WireMessage.
pub fn encrypt_chat_message(
    cipher: &ChaCha20Poly1305,
    msg: &ChatMessage,
) -> Option<EncryptedEnvelope> {
    let bytes = serde_json::to_vec(msg).ok()?;
    encrypt_bytes(cipher, &bytes)
}

pub fn decrypt_chat_message(
    cipher: &ChaCha20Poly1305,
    env: &EncryptedEnvelope,
) -> Option<ChatMessage> {
    let bytes = decrypt_bytes(cipher, env)?;
    serde_json::from_slice::<ChatMessage>(&bytes).ok()
}
