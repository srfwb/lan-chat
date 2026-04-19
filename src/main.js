// Accès aux APIs Tauri (exposées en global via withGlobalTauri: true)
const tauriCore = window.__TAURI__ && window.__TAURI__.core;
const tauriEvent = window.__TAURI__ && window.__TAURI__.event;

if (!tauriCore || !tauriEvent) {
  console.error(
    "[lan-chat] window.__TAURI__ indisponible — l'app doit être lancée via `npm run tauri dev`."
  );
}

const invoke = tauriCore ? tauriCore.invoke : async () => {
  throw new Error("Tauri indisponible");
};
const listen = tauriEvent ? tauriEvent.listen : async () => () => {};

// ───────────────────────── Helpers ─────────────────────────

function colorFromString(str) {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = str.charCodeAt(i) + ((hash << 5) - hash);
  }
  const hue = Math.abs(hash) % 360;
  return `hsl(${hue}, 68%, 58%)`;
}

function initials(name) {
  const parts = name.trim().split(/\s|-|_/).filter(Boolean);
  const letters = parts.slice(0, 2).map((p) => p[0].toUpperCase()).join("");
  return letters || "?";
}

function formatTime(ts) {
  const d = new Date(ts);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function shortPeerId(pid) {
  if (!pid || pid.length < 16) return pid || "?";
  return `${pid.slice(0, 10)}…${pid.slice(-4)}`;
}

// ───────────────────────── État ─────────────────────────

const ME = {
  id: null, // rempli par le PeerId libp2p dès que le nœud est prêt
  name: "…",
};

const seenPeers = new Set();
const seenMessageIds = new Set();
let isReady = false;
let pollToken = 0;
let hasAcceptedRoom = false;

// ───────────────────────── DOM ─────────────────────────

const el = {
  messages: document.getElementById("messages"),
  status: document.getElementById("status"),
  peersCount: document.getElementById("peers-count"),
  meName: document.getElementById("me-name"),
  meAvatar: document.getElementById("me-avatar"),
  form: document.getElementById("chat-form"),
  input: document.getElementById("chat-input"),
  send: document.querySelector(".composer__send"),
  roomBadge: document.getElementById("room-badge"),
  roomName: document.getElementById("room-name"),
  changeRoomBtn: document.getElementById("change-room-btn"),
  overlay: document.getElementById("room-overlay"),
  roomForm: document.getElementById("room-form"),
  roomInput: document.getElementById("room-code-input"),
  roomError: document.getElementById("room-error"),
};

function setStatus(text, variant = "info") {
  el.status.textContent = text;
  el.status.dataset.variant = variant;
}

function setInputEnabled(enabled) {
  el.input.disabled = !enabled;
  el.send.disabled = !enabled;
  if (enabled) el.input.focus();
}

function showOverlay() {
  el.overlay.hidden = false;
  el.roomInput.focus();
}

function hideOverlay() {
  el.overlay.hidden = true;
  hideRoomError();
}

function showRoomError(msg) {
  el.roomError.textContent = String(msg);
  el.roomError.hidden = false;
}

function hideRoomError() {
  el.roomError.hidden = true;
  el.roomError.textContent = "";
}

// ───────────────────────── Affichage ─────────────────────────

function registerPeer(senderId) {
  if (!senderId || senderId === ME.id) return;
  const before = seenPeers.size;
  seenPeers.add(senderId);
  if (seenPeers.size !== before) {
    el.peersCount.textContent = String(seenPeers.size);
  }
}

function appendMessage(msg, { mine }) {
  if (seenMessageIds.has(msg.id)) return;
  seenMessageIds.add(msg.id);

  const wrapper = document.createElement("div");
  wrapper.className = `message ${mine ? "message--mine" : "message--them"}`;

  const avatar = document.createElement("span");
  avatar.className = "message__avatar";
  avatar.style.background = colorFromString(msg.senderId);
  avatar.textContent = initials(msg.senderName);
  avatar.setAttribute("aria-hidden", "true");

  const bubble = document.createElement("div");
  bubble.className = "message__bubble";

  const head = document.createElement("div");
  head.className = "message__head";
  const sender = document.createElement("span");
  sender.className = "message__sender";
  sender.textContent = mine ? "Moi" : msg.senderName;
  const time = document.createElement("span");
  time.className = "message__time";
  time.textContent = formatTime(msg.timestamp);
  head.appendChild(sender);
  head.appendChild(time);

  const body = document.createElement("div");
  body.className = "message__body";
  body.textContent = msg.content;

  bubble.appendChild(head);
  bubble.appendChild(body);

  wrapper.appendChild(avatar);
  wrapper.appendChild(bubble);

  el.messages.appendChild(wrapper);
  el.messages.scrollTop = el.messages.scrollHeight;
}

function appendSystem(text, variant = "info") {
  const line = document.createElement("div");
  line.className = `system system--${variant}`;
  line.textContent = text;
  el.messages.appendChild(line);
  el.messages.scrollTop = el.messages.scrollHeight;
}

function appendSeparator(text) {
  const sep = document.createElement("div");
  sep.className = "history-separator";
  sep.setAttribute("aria-hidden", "true");
  const span = document.createElement("span");
  span.textContent = text;
  sep.appendChild(span);
  el.messages.appendChild(sep);
  el.messages.scrollTop = el.messages.scrollHeight;
}

/**
 * Récupère l'historique via invoke (pull, pas push) une fois que l'identité
 * locale (ME.id) est connue. Invoke est fiable (requête-réponse sync-like),
 * contrairement à un listen() qui risque de rater l'event si Rust l'émet avant
 * que le listener JS ne soit enregistré.
 */
async function renderHistoryThenSystem(roomName) {
  try {
    const history = await invoke("get_history");
    if (Array.isArray(history) && history.length > 0) {
      for (const m of history) {
        registerPeer(m.senderId);
        // NE PAS pré-ajouter à seenMessageIds : appendMessage le fait lui-même
        // après sa propre garde de dédup. Sinon, la garde retourne early.
        appendMessage(m, { mine: m.senderId === ME.id });
      }
      appendSeparator("— Nouveaux messages —");
    }
  } catch (e) {
    console.error("[lan-chat] get_history failed:", e);
  }
  appendSystem(
    `Connecté au salon « ${roomName} » (chiffré E2E). Découverte mDNS en cours.`
  );
}

// ───────────────────────── Envoi ─────────────────────────

async function sendMessage(content) {
  if (!ME.id) return; // nœud pas prêt
  const msg = {
    id: crypto.randomUUID(),
    senderName: ME.name,
    senderId: ME.id,
    content,
    timestamp: Date.now(),
  };

  // Affichage local immédiat (on filtrera l'écho réseau via senderId)
  appendMessage(msg, { mine: true });

  try {
    await invoke("send_message", { message: msg });
  } catch (e) {
    appendSystem(`Erreur d'envoi : ${e}`, "error");
  }
}

// ───────────────────────── Statut du nœud ─────────────────────────

function applyStatus(status) {
  if (!status || !status.kind) return false;

  if (status.kind === "awaitingRoom") {
    if (hasAcceptedRoom) {
      // Le set_room_code a déjà été envoyé — on ignore ce statut stale
      // et on continue le polling jusqu'au "ready".
      return false;
    }
    setStatus("En attente du code de salon…", "info");
    showOverlay();
    return true; // on arrête le polling tant que l'utilisateur n'a pas saisi un code
  }

  if (status.kind === "ready") {
    if (!isReady) {
      isReady = true;
      ME.id = status.peerId;
      el.meAvatar.style.background = colorFromString(ME.id);

      const roomName = status.roomName || "?";
      el.roomName.textContent = roomName;
      el.roomBadge.hidden = false;

      setStatus(`Salon « ${roomName} » · ${shortPeerId(ME.id)}`, "ready");
      hideOverlay();
      setInputEnabled(true);

      // Rend l'historique (invoke async) PUIS le message système "Connecté".
      // Ordre visuel : [ancien] → séparateur → [Connecté] → [nouveau live].
      renderHistoryThenSystem(roomName);
    }
    return true;
  }

  if (status.kind === "error") {
    setStatus(`Erreur : ${status.message}`, "error");
    appendSystem(
      `Le nœud libp2p n'a pas pu démarrer : ${status.message}`,
      "error"
    );
    return true;
  }

  // "initializing"
  return false;
}

async function pollStatus() {
  const MAX_ATTEMPTS = 150; // ~60 secondes
  const myToken = ++pollToken;
  for (let i = 0; i < MAX_ATTEMPTS; i++) {
    if (myToken !== pollToken) return; // un autre poll a été lancé
    try {
      const status = await invoke("get_node_status");
      if (applyStatus(status)) return;
    } catch (e) {
      console.error("[lan-chat] get_node_status failed:", e);
    }
    await new Promise((r) => setTimeout(r, 400));
  }
  setStatus("Délai d'attente dépassé — le nœud libp2p ne démarre pas.", "error");
}

// ───────────────────────── Salon ─────────────────────────

async function onRoomSubmit(e) {
  e.preventDefault();
  const code = el.roomInput.value.trim();
  if (!code) return;
  hideRoomError();
  try {
    await invoke("set_room_code", { code });
    hasAcceptedRoom = true;
    hideOverlay();
    setStatus("Connexion au salon…", "info");
    pollStatus();
  } catch (err) {
    console.error("[lan-chat] set_room_code failed:", err);
    showRoomError(err);
  }
}

async function onChangeRoom() {
  const ok = confirm(
    "Changer de salon supprime ta config locale. Tu devras relancer l'app pour choisir un nouveau code. Continuer ?"
  );
  if (!ok) return;
  try {
    await invoke("reset_room");
    setStatus("Salon effacé. Relance l'app pour en choisir un nouveau.", "warn");
    appendSystem(
      "Salon effacé. Ferme et relance l'app pour rejoindre un autre salon.",
      "info"
    );
    setInputEnabled(false);
  } catch (err) {
    appendSystem(`Erreur reset_room : ${err}`, "error");
  }
}

// ───────────────────────── Événements Tauri ─────────────────────────

listen("chat-message", (event) => {
  const msg = event.payload;
  registerPeer(msg.senderId);
  if (msg.senderId === ME.id) return; // écho de notre propre publish
  appendMessage(msg, { mine: false });
});

listen("node-ready", (event) => {
  const payload = event.payload || {};
  applyStatus({
    kind: "ready",
    peerId: payload.peerId,
    roomName: payload.roomName,
  });
});

listen("node-awaiting-room", () => {
  applyStatus({ kind: "awaitingRoom" });
});

listen("node-error", (event) => {
  applyStatus({ kind: "error", message: event.payload });
});

// ───────────────────────── Init ─────────────────────────

window.addEventListener("DOMContentLoaded", async () => {
  try {
    ME.name = await invoke("get_hostname");
  } catch (_e) {
    ME.name = "Inconnu";
  }
  el.meName.textContent = ME.name;
  el.meAvatar.textContent = initials(ME.name);

  el.form.addEventListener("submit", (e) => {
    e.preventDefault();
    const content = el.input.value.trim();
    if (!content) return;
    sendMessage(content);
    el.input.value = "";
  });

  el.roomForm.addEventListener("submit", onRoomSubmit);
  el.changeRoomBtn.addEventListener("click", onChangeRoom);

  pollStatus();
});
