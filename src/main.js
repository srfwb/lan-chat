// Accès aux APIs Tauri (exposées en global via withGlobalTauri: true)
const tauriCore = window.__TAURI__ && window.__TAURI__.core;
const tauriEvent = window.__TAURI__ && window.__TAURI__.event;
const tauriDialog = window.__TAURI__ && window.__TAURI__.dialog;
const tauriShell = window.__TAURI__ && window.__TAURI__.shell;

if (!tauriCore || !tauriEvent) {
  console.error(
    "[lan-chat] window.__TAURI__ indisponible — l'app doit être lancée via `npm run tauri dev`."
  );
}

const invoke = tauriCore ? tauriCore.invoke : async () => {
  throw new Error("Tauri indisponible");
};
const listen = tauriEvent ? tauriEvent.listen : async () => () => {};
const openFileDialog = tauriDialog ? tauriDialog.open : null;
const openWithSystem = tauriShell ? tauriShell.open : null;

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

function formatBytes(n) {
  if (!Number.isFinite(n) || n < 0) return "? o";
  if (n < 1024) return `${n} o`;
  const units = ["Ko", "Mo", "Go", "To"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 10 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

function fileIconForMime(mime) {
  if (!mime) return "📄";
  if (mime.startsWith("image/")) return "🖼";
  if (mime.startsWith("video/")) return "🎬";
  if (mime.startsWith("audio/")) return "🎵";
  if (mime.includes("pdf")) return "📕";
  if (mime.includes("zip") || mime.includes("compressed") || mime.includes("tar")) return "🗜";
  if (mime.startsWith("text/") || mime.includes("json") || mime.includes("xml")) return "📝";
  return "📄";
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
let activeSyncs = 0; // nombre de sync d'historique en cours (pour indicator UI)
let historySeparator = null; // référence DOM du séparateur "— Nouveaux messages —"

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
  attachBtn: document.getElementById("attach-btn"),
  dropOverlay: document.getElementById("drop-overlay"),
  roomBadge: document.getElementById("room-badge"),
  roomName: document.getElementById("room-name"),
  changeRoomBtn: document.getElementById("change-room-btn"),
  overlay: document.getElementById("room-overlay"),
  roomForm: document.getElementById("room-form"),
  roomInput: document.getElementById("room-code-input"),
  roomError: document.getElementById("room-error"),
  syncIndicator: document.getElementById("sync-indicator"),
};

// Circumférence de l'anneau de progression : 2π × 19px de rayon
const RING_CIRCUMFERENCE = 2 * Math.PI * 19;

function setStatus(text, variant = "info") {
  el.status.textContent = text;
  el.status.dataset.variant = variant;
}

function setInputEnabled(enabled) {
  el.input.disabled = !enabled;
  el.send.disabled = !enabled;
  if (el.attachBtn) el.attachBtn.disabled = !enabled;
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

/**
 * Construit le noeud DOM d'un message sans l'insérer.
 * Le dataset.timestamp est exposé pour permettre l'insertion triée par `insertHistoryMessage`.
 */
function buildMessageElement(msg, { mine }) {
  const wrapper = document.createElement("div");
  wrapper.className = `message ${mine ? "message--mine" : "message--them"}`;
  wrapper.dataset.timestamp = String(msg.timestamp);

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
  return wrapper;
}

/** Append à la fin de #messages. Utilisé pour messages live et historique initial. */
function appendMessage(msg, { mine }) {
  if (seenMessageIds.has(msg.id)) return;
  seenMessageIds.add(msg.id);
  const elem = buildMessageElement(msg, { mine });
  el.messages.appendChild(elem);
  el.messages.scrollTop = el.messages.scrollHeight;
}

/**
 * Insertion d'un message reçu via P2P history sync :
 * - Si un séparateur "— Nouveaux messages —" existe, on insère AVANT le séparateur,
 *   à la position chronologique correcte (trié par timestamp).
 * - Sinon (edge case : sync avant le premier render), append à la fin.
 */
function insertHistoryMessage(msg, { mine }) {
  if (seenMessageIds.has(msg.id)) return;
  seenMessageIds.add(msg.id);
  const elem = buildMessageElement(msg, { mine });

  if (!historySeparator || !historySeparator.isConnected) {
    el.messages.appendChild(elem);
    el.messages.scrollTop = el.messages.scrollHeight;
    return;
  }

  // Walk backward depuis le séparateur jusqu'au premier message avec ts <= msg.timestamp
  let cursor = historySeparator.previousElementSibling;
  while (cursor) {
    const ts = Number(cursor.dataset?.timestamp);
    if (!Number.isNaN(ts) && ts <= msg.timestamp) {
      cursor.after(elem);
      return;
    }
    cursor = cursor.previousElementSibling;
  }
  // Plus vieux que tout message visible : tout en haut
  el.messages.insertBefore(elem, el.messages.firstChild);
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
  historySeparator = sep; // ← référence pour l'insertion triée des syncs ultérieures
  el.messages.scrollTop = el.messages.scrollHeight;
}

function updateSyncIndicator() {
  if (!el.syncIndicator) return;
  el.syncIndicator.hidden = activeSyncs === 0;
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
    const hasHistory = Array.isArray(history) && history.length > 0;
    if (hasHistory) {
      for (const m of history) {
        registerPeer(m.senderId);
        // NE PAS pré-ajouter à seenMessageIds : appendMessage le fait lui-même
        // après sa propre garde de dédup. Sinon, la garde retourne early.
        appendMessage(m, { mine: m.senderId === ME.id });
      }
    }
    // Séparateur rendu systématiquement — il sert d'ancre à insertHistoryMessage
    // lors des syncs P2P ultérieures, même quand l'historique local est vide.
    appendSeparator(hasHistory ? "— Nouveaux messages —" : "— Début de la session —");
  } catch (e) {
    console.error("[lan-chat] get_history failed:", e);
  }
  appendSystem(
    `Connecté au salon « ${roomName} » (chiffré E2E). Découverte mDNS en cours.`
  );
}

// ───────────────────────── Fichiers ─────────────────────────

/** Track des offers reçues côté UI : file_id → { senderId, filename, size, mime } */
const fileOffers = new Map();
/** file_id → DOM de la bulle */
const fileBubbles = new Map();

/**
 * Construit la structure d'une bulle fichier. Les états sont gérés via data-state
 * et la mise à jour du SVG ring (stroke-dashoffset) + des textes.
 */
function buildFileBubbleElement(offer, { mine, state }) {
  const wrapper = document.createElement("div");
  wrapper.className = `message ${mine ? "message--mine" : "message--them"}`;
  wrapper.dataset.timestamp = String(offer.timestamp);

  const avatar = document.createElement("span");
  avatar.className = "message__avatar";
  avatar.style.background = colorFromString(offer.senderId || ME.id);
  avatar.textContent = initials(offer.senderName || ME.name);
  avatar.setAttribute("aria-hidden", "true");

  const bubble = document.createElement("div");
  bubble.className = "message__bubble";

  const head = document.createElement("div");
  head.className = "message__head";
  const sender = document.createElement("span");
  sender.className = "message__sender";
  sender.textContent = mine ? "Moi" : offer.senderName;
  const time = document.createElement("span");
  time.className = "message__time";
  time.textContent = formatTime(offer.timestamp);
  head.appendChild(sender);
  head.appendChild(time);

  // Cœur de la bulle fichier
  const fileRow = document.createElement("div");
  fileRow.className = "file-bubble";
  fileRow.dataset.fileId = offer.id;
  fileRow.dataset.state = state;
  fileRow.dataset.mine = String(!!mine);
  fileRow.dataset.senderPeer = offer.senderId;

  const action = document.createElement("button");
  action.type = "button";
  action.className = "file-bubble__action";
  action.setAttribute("aria-label", "Télécharger le fichier");

  // Anneau SVG (track + progress)
  action.innerHTML = `
    <svg class="file-bubble__ring" width="44" height="44" viewBox="0 0 44 44" aria-hidden="true">
      <circle class="file-bubble__ring-track" cx="22" cy="22" r="19"></circle>
      <circle class="file-bubble__ring-progress" cx="22" cy="22" r="19"
              stroke-dasharray="${RING_CIRCUMFERENCE.toFixed(2)}"
              stroke-dashoffset="${RING_CIRCUMFERENCE.toFixed(2)}"></circle>
    </svg>
    <span class="file-bubble__icon"></span>
  `;

  const info = document.createElement("div");
  info.className = "file-bubble__info";
  const name = document.createElement("span");
  name.className = "file-bubble__name";
  name.textContent = offer.filename;
  name.title = offer.filename;
  const meta = document.createElement("span");
  meta.className = "file-bubble__meta";
  info.appendChild(name);
  info.appendChild(meta);

  fileRow.appendChild(action);
  fileRow.appendChild(info);

  bubble.appendChild(head);
  bubble.appendChild(fileRow);

  wrapper.appendChild(avatar);
  wrapper.appendChild(bubble);

  // Wiring handler selon l'état initial
  applyFileBubbleState(fileRow, offer, state);

  return wrapper;
}

function appendFileBubble(offer, { mine, state }) {
  if (fileBubbles.has(offer.id)) return;
  const wrapper = buildFileBubbleElement(offer, { mine, state });
  el.messages.appendChild(wrapper);
  fileBubbles.set(offer.id, wrapper.querySelector(".file-bubble"));
  el.messages.scrollTop = el.messages.scrollHeight;
}

/** Met à jour l'icône, meta et l'anneau en fonction de l'état courant. */
function applyFileBubbleState(fileRow, offer, state) {
  fileRow.dataset.state = state;
  const iconEl = fileRow.querySelector(".file-bubble__icon");
  const metaEl = fileRow.querySelector(".file-bubble__meta");
  const actionEl = fileRow.querySelector(".file-bubble__action");

  actionEl.onclick = null;

  if (state === "available") {
    iconEl.textContent = "⬇";
    metaEl.textContent = `${formatBytes(offer.size)} · à télécharger`;
    setRingPct(fileRow, 0);
    actionEl.onclick = () => onDownloadClick(offer);
    actionEl.setAttribute("aria-label", "Télécharger le fichier");
    actionEl.disabled = false;
  } else if (state === "sent") {
    iconEl.textContent = fileIconForMime(offer.mime);
    metaEl.textContent = `${formatBytes(offer.size)} · Envoyé`;
    setRingPct(fileRow, 100);
    actionEl.disabled = true;
    actionEl.setAttribute("aria-label", "Fichier envoyé");
  } else if (state === "downloading") {
    iconEl.textContent = "⏸";
    metaEl.textContent = `0 / ${formatBytes(offer.size)} · 0 %`;
    setRingPct(fileRow, 0);
    actionEl.disabled = true; // annulation : hors scope v1
    actionEl.setAttribute("aria-label", "Téléchargement en cours");
  } else if (state === "completed") {
    iconEl.textContent = fileIconForMime(offer.mime);
    metaEl.textContent = `${formatBytes(offer.size)} · Ouvrir`;
    setRingPct(fileRow, 100);
    actionEl.disabled = false;
    actionEl.onclick = () => onOpenClick(fileRow);
    actionEl.setAttribute("aria-label", "Ouvrir le fichier");
  } else if (state === "error") {
    iconEl.textContent = "⚠";
    // meta est rempli par updateFileBubbleError
    actionEl.disabled = false;
    actionEl.onclick = () => onDownloadClick(offer); // retry
    actionEl.setAttribute("aria-label", "Réessayer le téléchargement");
  }
}

function setRingPct(fileRow, pct) {
  const ring = fileRow.querySelector(".file-bubble__ring-progress");
  if (!ring) return;
  const clamped = Math.max(0, Math.min(100, pct));
  const offset = RING_CIRCUMFERENCE * (1 - clamped / 100);
  ring.style.strokeDashoffset = String(offset);
}

function updateFileBubbleProgress(fileId, received, total) {
  const fileRow = fileBubbles.get(fileId);
  if (!fileRow) return;
  const pct = total > 0 ? (received / total) * 100 : 0;
  setRingPct(fileRow, pct);
  const metaEl = fileRow.querySelector(".file-bubble__meta");
  if (metaEl) {
    metaEl.textContent = `${formatBytes(received)} / ${formatBytes(total)} · ${Math.round(pct)} %`;
  }
}

function updateFileBubbleCompleted(fileId, localPath) {
  const fileRow = fileBubbles.get(fileId);
  if (!fileRow) return;
  const offer = fileOffers.get(fileId);
  if (!offer) return;
  fileRow.dataset.localPath = localPath;
  applyFileBubbleState(fileRow, offer, "completed");
}

function updateFileBubbleError(fileId, message) {
  const fileRow = fileBubbles.get(fileId);
  if (!fileRow) return;
  const offer = fileOffers.get(fileId);
  if (!offer) return;
  applyFileBubbleState(fileRow, offer, "error");
  const metaEl = fileRow.querySelector(".file-bubble__meta");
  if (metaEl) metaEl.textContent = `Erreur : ${message}`;
  setRingPct(fileRow, 0);
}

async function onDownloadClick(offer) {
  const fileRow = fileBubbles.get(offer.id);
  if (!fileRow) return;
  applyFileBubbleState(fileRow, offer, "downloading");
  try {
    await invoke("download_file", {
      fileId: offer.id,
      senderPeer: offer.senderId,
    });
  } catch (e) {
    updateFileBubbleError(offer.id, String(e));
  }
}

async function onOpenClick(fileRow) {
  const path = fileRow.dataset.localPath;
  if (!path) return;
  if (!openWithSystem) {
    appendSystem("Ouverture impossible : plugin shell indisponible.", "error");
    return;
  }
  try {
    await openWithSystem(path);
  } catch (e) {
    appendSystem(`Ouverture du fichier échouée : ${e}`, "error");
  }
}

async function sendFile(path) {
  if (!ME.id) {
    appendSystem("Impossible d'envoyer un fichier : le nœud n'est pas encore prêt.", "error");
    return;
  }
  const fileId = crypto.randomUUID();
  try {
    const offer = await invoke("offer_file", {
      path,
      fileId,
      senderName: ME.name,
    });
    fileOffers.set(offer.id, offer);
    appendFileBubble(offer, { mine: true, state: "sent" });
  } catch (e) {
    appendSystem(`Envoi du fichier échoué : ${e}`, "error");
  }
}

async function onAttachClick() {
  if (!openFileDialog) {
    appendSystem("Sélecteur de fichier indisponible.", "error");
    return;
  }
  try {
    const picked = await openFileDialog({ multiple: false, directory: false });
    if (typeof picked === "string" && picked.length > 0) {
      await sendFile(picked);
    }
  } catch (e) {
    appendSystem(`Sélection du fichier échouée : ${e}`, "error");
  }
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

      // Défense : si un sync d'une session précédente n'avait pas émis son "end"
      // (abort swarm sans LeaveRoom), on nettoie le compteur ici.
      activeSyncs = 0;
      updateSyncIndicator();

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
    // Race possible : le node-ready event peut arriver AVANT que cette ligne
    // soit atteinte. Ne pas écraser un statut "ready" déjà posé.
    if (!isReady) {
      setStatus("Connexion au salon…", "info");
    }
    pollStatus();
  } catch (err) {
    console.error("[lan-chat] set_room_code failed:", err);
    showRoomError(err);
  }
}

async function onChangeRoom() {
  const ok = confirm(
    "Changer de salon ? L'historique du salon actuel est conservé sur cette machine — tu pourras y revenir en retapant le même code."
  );
  if (!ok) return;
  try {
    await invoke("leave_room");
    // Le listener node-awaiting-room s'occupe du reset UI + affichage de l'overlay
  } catch (err) {
    appendSystem(`Erreur leave_room : ${err}`, "error");
  }
}

/**
 * Reset complet de l'UI quand le swarm est abandonné (leave_room).
 * Appelé à la réception de l'event node-awaiting-room après le départ du salon.
 */
function resetUiForAwaitingRoom() {
  el.messages.innerHTML = "";
  seenMessageIds.clear();
  seenPeers.clear();
  fileOffers.clear();
  fileBubbles.clear();
  el.peersCount.textContent = "0";
  el.roomBadge.hidden = true;
  el.roomName.textContent = "—";
  isReady = false;
  ME.id = null;
  hasAcceptedRoom = false;
  el.meAvatar.style.background = "";
  setInputEnabled(false);
  historySeparator = null;
  activeSyncs = 0;
  updateSyncIndicator();
  if (el.dropOverlay) el.dropOverlay.hidden = true;
}

// ───────────────────────── Événements Tauri ─────────────────────────

listen("chat-message", (event) => {
  const msg = event.payload;
  registerPeer(msg.senderId);
  if (msg.senderId === ME.id) return; // écho de notre propre publish
  appendMessage(msg, { mine: false });
});

listen("file-offer", (event) => {
  const offer = event.payload;
  if (!offer || !offer.id) return;
  registerPeer(offer.senderId);
  // Écho de notre propre publish : on a déjà affiché la bulle "Envoyé" localement
  if (offer.senderId === ME.id) return;
  fileOffers.set(offer.id, offer);
  appendFileBubble(offer, { mine: false, state: "available" });
});

listen("file-progress", (event) => {
  const { fileId, received, total } = event.payload || {};
  if (!fileId) return;
  updateFileBubbleProgress(fileId, received, total);
});

listen("file-completed", (event) => {
  const { fileId, localPath } = event.payload || {};
  if (!fileId) return;
  updateFileBubbleCompleted(fileId, localPath);
});

listen("file-error", (event) => {
  const { fileId, message } = event.payload || {};
  if (!fileId) return;
  updateFileBubbleError(fileId, message || "erreur inconnue");
});

// Drag & drop — Tauri v2 émet sur la fenêtre : hover / drop / cancelled.
listen("tauri://drag-enter", () => {
  if (!isReady) return;
  if (el.dropOverlay) el.dropOverlay.hidden = false;
});
listen("tauri://drag-over", () => {
  if (!isReady) return;
  if (el.dropOverlay) el.dropOverlay.hidden = false;
});
listen("tauri://drag-leave", () => {
  if (el.dropOverlay) el.dropOverlay.hidden = true;
});
listen("tauri://drag-drop", async (event) => {
  if (el.dropOverlay) el.dropOverlay.hidden = true;
  if (!isReady) return;
  const paths = event.payload?.paths;
  if (!Array.isArray(paths) || paths.length === 0) return;
  for (const p of paths) {
    await sendFile(p);
  }
});

// Message reçu via P2P history sync : insertion triée par timestamp avant le séparateur.
listen("history-message", (event) => {
  const msg = event.payload;
  registerPeer(msg.senderId);
  insertHistoryMessage(msg, { mine: msg.senderId === ME.id });
});

listen("history-sync-start", () => {
  activeSyncs++;
  updateSyncIndicator();
});

listen("history-sync-end", () => {
  activeSyncs = Math.max(0, activeSyncs - 1);
  updateSyncIndicator();
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
  resetUiForAwaitingRoom();
  applyStatus({ kind: "awaitingRoom" });
  // Relance le polling pour détecter le prochain Ready quand l'utilisateur
  // soumettra un nouveau code (le polling précédent a rendu true sur awaitingRoom).
  pollStatus();
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
  if (el.attachBtn) el.attachBtn.addEventListener("click", onAttachClick);

  pollStatus();
});
