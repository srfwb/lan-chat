# lan-chat

A zero-config, end-to-end encrypted LAN messenger. No server, no account, no sign-up — share a room code with people on the same Wi-Fi and chat.

Built with [Tauri](https://tauri.app/) (Rust + web) and [libp2p](https://libp2p.io/).

## Features

- **Pure peer-to-peer** — peers find each other via mDNS, messages travel over libp2p gossipsub
- **End-to-end encrypted** — ChaCha20-Poly1305 key derived from the shared room code; only peers with the same code can read each other
- **Persistent identity** — stable ed25519 PeerId across sessions
- **Encrypted local history** — last 500 messages per room, persisted on disk under the room key
- **History sync** — late joiners pull past messages from peers already in the room (libp2p `request_response`)
- **Hot room switching** — swap rooms without restarting the app
- **Strict CSP** on the webview to keep the frontend locked down

## Install

Grab the latest installer from [**Releases**](https://github.com/srfwb/lan-chat/releases).

Windows only for now. macOS and Linux builds are on the roadmap.

> Windows will prompt for firewall access on first launch. Allow it on **Private networks** so mDNS and gossipsub can reach your LAN.

## Usage

1. Launch the app on two machines on the same Wi-Fi
2. Pick a room code on both — anything, as long as it matches (e.g. `alpha-cafe-7`)
3. Start chatting

Different code ⇒ different room ⇒ you won't see each other. Same code ⇒ instant P2P chat, messages never leave your LAN.

## How it works

- **Discovery** — libp2p mDNS on `_libp2p._udp.local.`
- **Transport** — TCP, Noise-encrypted, Yamux-multiplexed
- **Messaging** — libp2p gossipsub, topic = `lan-chat-v1-<sha256_16hex(room_code)>`
- **Application crypto** — every message is wrapped in an `EncryptedEnvelope { nonce, ciphertext }` encrypted with ChaCha20-Poly1305, key = `SHA256("lan-chat-v1|key|" + room_code)`
- **Identity** — ed25519 keypair in `%APPDATA%\com.srfwb.lanchat\identity.key`
- **History** — per-room `history/<room-hash>.bin`, same AEAD as transport, capped at 500 messages
- **History sync** — on mDNS discovery, one `HistoryRequest` per peer over `/lan-chat/history-sync/1`; response is a `Vec<EncryptedEnvelope>`, still sealed with the room key

Design notes live under [`docs/superpowers/specs/`](docs/superpowers/specs/).

## Development

Requirements:

- [Node.js](https://nodejs.org/) 20+
- [Rust](https://rustup.rs/) stable (1.77+)
- (Windows) [WiX Toolset v3](https://github.com/wixtoolset/wix3/releases) for MSI builds

```bash
git clone https://github.com/srfwb/lan-chat
cd lan-chat
npm install
npm run tauri dev    # dev server with hot-reload
npm run tauri build  # production bundle (installer + raw exe)
```

## Roadmap

- [ ] macOS + Linux builds via GitHub Actions
- [ ] Hot re-key without rediscovery blackout (~3-5 s today)
- [ ] Presence indicator (who's online)
- [ ] Rate-limiting and DoS hardening on the sync protocol
- [ ] Configurable message retention per room

## License

[MIT](LICENSE) © srfwb
