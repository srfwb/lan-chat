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
  id: null,   // rempli par le PeerId libp2p dès que le nœud est prêt
  name: "…",
};

const seenPeers = new Set();
const seenMessageIds = new Set();
let isReady = false;

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

  if (status.kind === "ready") {
    if (!isReady) {
      isReady = true;
      ME.id = status.peerId;
      el.meAvatar.style.background = colorFromString(ME.id);
      setStatus(`Nœud P2P actif · ${shortPeerId(ME.id)}`, "ready");
      setInputEnabled(true);
      appendSystem("Nœud libp2p prêt. Découverte mDNS en cours — en attente de pairs…");
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
  const MAX_ATTEMPTS = 150; // ~60 secondes (démarrage libp2p peut prendre quelques secondes)
  for (let i = 0; i < MAX_ATTEMPTS; i++) {
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

// ───────────────────────── Événements Tauri (backup) ─────────────────────────

listen("chat-message", (event) => {
  const msg = event.payload;
  registerPeer(msg.senderId);
  if (msg.senderId === ME.id) return; // écho de notre propre publish
  appendMessage(msg, { mine: false });
});

listen("node-ready", (event) => {
  applyStatus({ kind: "ready", peerId: event.payload });
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

  pollStatus();
});
