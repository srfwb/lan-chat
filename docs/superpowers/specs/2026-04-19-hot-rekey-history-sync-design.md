# Design — Hot re-key + P2P history sync

## Context

The previous PR (`feature/libp2p-migration`) shipped an end-to-end encrypted LAN chat with persistent identity, room-gated access, strict CSP, and local message history. Its accepted limitations were:

1. **Changing rooms requires an app restart** — no hot re-key; the swarm is built once for a single `RoomConfig` and cannot swap.
2. **No P2P history sync** — late joiners do not see messages exchanged before they arrived.

This design lifts both limitations in a single PR while preserving the security posture.

## Goals

- User can switch rooms from within the app without restarting.
- New peers joining a room automatically receive existing peers' history *(bounded by the 500-message local cap)*.
- No regression in the existing security model.
- Minimal UI churn : reuse the existing overlay for the "enter room code" step; add a discrete sync indicator.

## Non-goals

- Cross-session presence awareness (who is online right now).
- Rate limiting.
- Replay protection at the application layer *(already covered by AEAD per-message nonces)*.
- A UI for "select from past rooms" *(user re-types the code for now)*.

---

## Architecture — `SwarmManager` coordinator

The current architecture spawns a single `run_swarm` task for the lifetime of the process. We replace this with a **manager loop** that owns the current swarm task and can restart it on demand.

```
Frontend               Manager task              Swarm task N
────────               ────────────              ─────────────
set_room_code(X)  →  [StartRoom(X)]    → abort N-1 ──→ run_swarm(X)
leave_room()      →  [LeaveRoom]       → abort N   ──→ (gone)
                     (awaits next cmd)
```

### Shared state refactor

Currently global, one-shot:

```rust
static HISTORY: OnceLock<Mutex<VecDeque<ChatMessage>>> = OnceLock::new();
static HISTORY_STORE: OnceLock<Arc<HistoryStore>> = OnceLock::new();
static PUBLISHER_TX: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
static ROOM_SETUP_TX: OnceLock<Mutex<Option<oneshot::Sender<RoomConfig>>>> = OnceLock::new();
```

Refactored, resettable:

```rust
static HISTORY: OnceLock<Mutex<Option<VecDeque<ChatMessage>>>> = OnceLock::new();
static HISTORY_STORE: OnceLock<Mutex<Option<Arc<HistoryStore>>>> = OnceLock::new();
static PUBLISHER_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<String>>>> = OnceLock::new();
static MANAGER_TX: OnceLock<mpsc::UnboundedSender<ManagerCmd>> = OnceLock::new();
```

`MANAGER_TX` replaces the one-shot with a persistent multi-command channel.

### Commands handled by the manager

```rust
enum ManagerCmd {
    StartRoom(RoomConfig),
    LeaveRoom,
}
```

```rust
// Coordinator loop (spawned once in setup())
let mut current: Option<tauri::async_runtime::JoinHandle<()>> = None;

while let Some(cmd) = manager_rx.recv().await {
    match cmd {
        ManagerCmd::StartRoom(config) => {
            if let Some(h) = current.take() { h.abort(); }
            clear_shared_state();
            set_node_status(NodeStatus::Initializing);
            current = Some(tauri::async_runtime::spawn(async move {
                if let Err(e) = run_swarm(handle, keypair.clone(), config).await {
                    set_node_status(NodeStatus::Error { message: e.to_string() });
                }
            }));
        }
        ManagerCmd::LeaveRoom => {
            if let Some(h) = current.take() { h.abort(); }
            clear_shared_state();
            set_node_status(NodeStatus::AwaitingRoom);
            let _ = app.emit("node-awaiting-room", ());
        }
    }
}
```

`clear_shared_state()` empties `HISTORY`, drops `HISTORY_STORE`, closes `PUBLISHER_TX`, clears the per-session sync set.

---

## Feature 1 — Hot re-key (room swap without restart)

### New Tauri command

```rust
#[tauri::command]
fn leave_room(app: AppHandle) -> Result<(), String> {
    delete_room_config(&app)?;
    MANAGER_TX.get().unwrap().send(ManagerCmd::LeaveRoom)?;
    Ok(())
}
```

### Reworked `set_room_code`

```rust
#[tauri::command]
fn set_room_code(app: AppHandle, code: String) -> Result<(), String> {
    // …validation unchanged…
    save_room_config(&app, &config)?;
    MANAGER_TX.get().unwrap()
        .send(ManagerCmd::StartRoom(config))
        .map_err(|e| format!("manager channel closed: {}", e))?;
    Ok(())
}
```

Note: `set_room_code` is now **idempotent from the manager's view** — sending `StartRoom` twice is safe (the manager aborts the previous swarm and starts fresh).

### Frontend changes

- `onChangeRoom()` calls `leave_room()` → no more "restart required" message
- On `node-awaiting-room`, the frontend clears the DOM `#messages`, resets `seenMessageIds`, `seenPeers`, `isReady`, `ME.id` → shows overlay
- On subsequent `node-ready`, the normal flow runs *(status, history, system message)*

### Downtime between rooms

~3–5 s between `leave_room` and `node-ready`:
- <100 ms : swarm abort + state cleanup
- 1–2 s : new swarm build, TCP `listen_on`, gossipsub subscribe
- 2–5 s : mDNS rediscovery of peers in the new room

The PeerId is stable across swaps *(identity ed25519 persisted since the previous PR)*.

---

## Feature 2 — P2P history sync

### New behaviour

Add `request_response::Behaviour` with a custom codec to the existing `ChatBehaviour`:

```rust
#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    history_sync: request_response::Behaviour<HistoryCodec>,
}
```

### Protocol

Protocol id : `/lan-chat/history-sync/1`

**Request** *(small, no secret)* :

```rust
#[derive(Serialize, Deserialize)]
struct HistoryRequest {
    version: u32, // 1
}
```

**Response** *(encrypted with the room key)* :

```rust
#[derive(Serialize, Deserialize)]
struct HistoryResponse {
    version: u32,
    messages: Vec<EncryptedEnvelope>, // reuses the envelope type used on gossipsub
}
```

### Codec

Serde JSON wrapped in a `request_response::Codec` implementation. JSON keeps the payload human-inspectable during debug.

### When sync fires

- mDNS `Discovered` event fires with peer X
- If X ∉ `synced_peers` (per-session `HashSet<PeerId>`) : send `HistoryRequest` → insert X into `synced_peers`
- This guarantees exactly one sync attempt per (peer, session) couple

`synced_peers` is cleared on `ManagerCmd::LeaveRoom` *(part of `clear_shared_state`)*.

### Handling a request (server side)

1. Snapshot the current `HISTORY` (clone under lock)
2. For each `ChatMessage`, re-encrypt into an `EncryptedEnvelope` using the current cipher *(same nonce-per-message generation as the publish path)*
3. Return a `HistoryResponse { version: 1, messages: envelopes }`

### Handling a response (client side)

1. For each `envelope`, attempt `cipher.decrypt(nonce, ct)` → on failure, drop silently *(wrong room key, tampering, etc.)*
2. Deserialize `ChatMessage`
3. Check `seenMessageIds` *(or local history ids)* for dedup
4. If new : append to `HISTORY`, persist *(triggers `HistoryStore::save`)*, emit `chat-message` event to UI

UI inserts messages in their **timestamp-sorted position**, not just at the bottom — so historical messages appear chronologically among existing ones if they arrive late.

### Bounded by local cap

Each response contains at most 500 messages (the local `HISTORY_CAP`). A chatty session with 3 peers meeting ⇒ 3 syncs × ~150 KB = ~450 KB over libp2p. Acceptable on LAN.

---

## Security model

No regression vs the previous PR. Added coverage :

| Threat | Protection |
|---|---|
| Attacker on the LAN discovers us via mDNS and requests our history | Response is encrypted with the room key; attacker decrypts to noise → drop |
| Attacker forges a history response to poison our DB | AEAD tag mismatch → silent drop per envelope |
| Replay of historical messages via sync flood | Dedup by message ID at ingest |
| Man-in-the-middle between two legitimate peers | Noise transport already protects peer-to-peer traffic |
| Room switch leaks memory of old room | `clear_shared_state()` drops `HISTORY`, `HISTORY_STORE`, cipher; Rust Drop runs on the old swarm |

### Room key derivation

Unchanged — reuses the existing `derive_room(code)` that derives both the gossipsub topic and the ChaCha20-Poly1305 key from the user-supplied code.

---

## Data flow

### Room switch

```
UI  [Changer de salon]
 │
 ▼
confirm() → invoke("leave_room")
                │
                ▼
   MANAGER_TX.send(LeaveRoom)
                │
                ▼
         Manager: abort ── Swarm N ──► dropped
                │                       │
                │                       ▼
                │                   Drop runs
                │             (TCP closed, mDNS unreg)
                ▼
    clear_shared_state()  →  set_node_status(AwaitingRoom)
                │
                ▼
          emit node-awaiting-room
                │
                ▼
   Frontend clears DOM + shows overlay
                │
                ▼
   [User types new code] invoke("set_room_code", {code})
                │
                ▼
   MANAGER_TX.send(StartRoom(config))
                │
                ▼
         Manager: spawn Swarm N+1
                │
                ▼
    run_swarm ... listen_on ... set Ready
                │
                ▼
           emit node-ready
                │
                ▼
   Frontend pulls history via get_history, displays
```

### History sync on discovery

```
Peer X joins LAN + subscribes to room topic
 │
 ▼
mDNS emits Discovered(X)
 │
 ▼
swarm's select! loop:
  swarm.behaviour_mut().gossipsub.add_explicit_peer(&X)
  if synced_peers.insert(X) returned true (newly added):
      request_response.send_request(&X, HistoryRequest { version: 1 })
      sync_in_progress += 1
      emit history-sync-start(peerId = X)
 │
 ▼
On peer X side: request_response fires Request event
      response = build_response_from_current_history()
      request_response.send_response(channel, response)
 │
 ▼
Back on client: request_response fires Response event
      for envelope in response.messages:
          if let Ok(plaintext) = cipher.decrypt(envelope):
              msg = deserialize(plaintext)
              if seen.insert(msg.id):
                  HISTORY.push(msg)
                  HISTORY_STORE.save(...)
                  emit chat-message(msg)
      sync_in_progress -= 1
      emit history-sync-end
 │
 ▼
UI updates: status indicator disappears when sync_in_progress == 0
```

---

## UI changes

| Change | Where |
|---|---|
| "Changer de salon" button now calls `leave_room` instead of showing "restart required" | `main.js` – `onChangeRoom` |
| On `node-awaiting-room`, reset DOM + show overlay | `main.js` – listener + `applyStatus` |
| New status indicator: "Synchronisation de l'historique…" with subtle pulsing dot | `index.html` element, `styles.css` class `.sync-indicator`, `main.js` counter |
| Messages received via sync are inserted in chronological order | `main.js` – `insertMessageSorted` replaces `appendMessage` for history-sync arrivals only (live messages stay append-only) |

The room overlay, input, avatar, and footer are unchanged.

---

## Files changed

| File | Changes |
|---|---|
| `src-tauri/Cargo.toml` | Add feature `request-response` to `libp2p` (default codec used) |
| `src-tauri/src/lib.rs` | Manager loop; `ManagerCmd` enum; refactor statics to `Mutex<Option<T>>`; `history_sync` behaviour + codec; `leave_room` command; request/response handlers; `synced_peers` tracking; `clear_shared_state` |
| `src-tauri/src/history_codec.rs` *(new)* | `HistoryCodec` impl of `request_response::Codec` with JSON serde |
| `src/main.js` | `leave_room` call; DOM reset on `node-awaiting-room`; `history-sync-start`/`end` listeners; `insertMessageSorted` helper |
| `src/index.html` | Small sync-indicator element inside the status bar |
| `src/styles.css` | `.sync-indicator` class (pulsing, discrete) |

---

## Test plan

### Hot re-key
1. Launch, room `test-a`, send 3 messages.
2. Click "Changer de salon" → confirm → overlay appears with `#messages` cleared.
3. Type `test-b` → Rejoindre → after ~3-5 s, chat shows `Salon test-b` and (if it exists) test-b's history.
4. Click "Changer de salon" again → enter `test-a` → test-a's history reappears *(persisted per room-hash)*.
5. Rust terminal shows `[lan-chat] Listening on …` events for each new swarm.

### History sync
6. PC-A runs app with room `test-a`, sends 50 messages over time.
7. PC-B (fresh) launches with room `test-a`.
8. Within ~3 s of PC-B's mDNS discovering PC-A :
   - Status bar shows "Synchronisation de l'historique…"
   - The 50 messages appear in PC-B's chat, in chronological order
   - Status indicator disappears once the sync completes
9. PC-C joins later, discovers both A and B → receives same 50 messages (deduped — no duplicates).

### Security
10. PC-X on same LAN, different room code `attacker`. `mDNS` discovers it normally.
11. PC-X sends a `HistoryRequest` to PC-A.
12. PC-A responds; PC-X's `cipher.decrypt` fails on every envelope → PC-X ends up with 0 messages from the "leak".

### Negative
13. Abort mid-sync (close PC-A mid-response) → PC-B logs `request_response` failure, indicator disappears, no state corruption.
14. Malformed response (manually crafted via test harness) → AEAD fails → drop, no panic.

---

## Rollout plan

2 commits on a dedicated branch `feature/hot-rekey-history-sync` :

1. `refactor(state): replace OnceLock with Mutex<Option> for resettable globals + SwarmManager coordinator loop`
2. `feat(chat): hot re-key rooms + P2P history sync via libp2p request_response`

Then merge-commit back to `main` (same discipline as the libp2p-migration PR) to preserve the two logical steps.
