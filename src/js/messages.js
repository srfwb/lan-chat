import { el } from "./dom.js";
import { ME, seenMessageIds, state } from "./state.js";
import { colorFromString, formatTime, initials } from "./utils.js";
import { invoke } from "./tauri-api.js";
import { registerPeer } from "./peers.js";

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

export function appendMessage(msg, { mine }) {
  if (seenMessageIds.has(msg.id)) return;
  seenMessageIds.add(msg.id);
  el.messages.appendChild(buildMessageElement(msg, { mine }));
  el.messages.scrollTop = el.messages.scrollHeight;
}

/// Message reçu via P2P history sync : insertion triée AVANT le séparateur.
/// Si aucun séparateur (edge : sync avant le premier render), append à la fin.
export function insertHistoryMessage(msg, { mine }) {
  if (seenMessageIds.has(msg.id)) return;
  seenMessageIds.add(msg.id);
  const elem = buildMessageElement(msg, { mine });

  if (!state.historySeparator || !state.historySeparator.isConnected) {
    el.messages.appendChild(elem);
    el.messages.scrollTop = el.messages.scrollHeight;
    return;
  }

  let cursor = state.historySeparator.previousElementSibling;
  while (cursor) {
    const ts = Number(cursor.dataset?.timestamp);
    if (!Number.isNaN(ts) && ts <= msg.timestamp) {
      cursor.after(elem);
      return;
    }
    cursor = cursor.previousElementSibling;
  }
  el.messages.insertBefore(elem, el.messages.firstChild);
}

export function appendSystem(text, variant = "info") {
  const line = document.createElement("div");
  line.className = `system system--${variant}`;
  line.textContent = text;
  el.messages.appendChild(line);
  el.messages.scrollTop = el.messages.scrollHeight;
}

export function appendSeparator(text) {
  const sep = document.createElement("div");
  sep.className = "history-separator";
  sep.setAttribute("aria-hidden", "true");
  const span = document.createElement("span");
  span.textContent = text;
  sep.appendChild(span);
  el.messages.appendChild(sep);
  state.historySeparator = sep; // ancre pour insertHistoryMessage
  el.messages.scrollTop = el.messages.scrollHeight;
}

export function updateSyncIndicator() {
  if (!el.syncIndicator) return;
  el.syncIndicator.hidden = state.activeSyncs === 0;
}

/// Pull de l'historique (invoke, fiable) puis message « Connecté ».
/// Push-via-event serait racy : le listener peut rater l'event si Rust émet avant.
export async function renderHistoryThenSystem(roomName) {
  try {
    const history = await invoke("get_history");
    const hasHistory = Array.isArray(history) && history.length > 0;
    if (hasHistory) {
      for (const m of history) {
        registerPeer(m.senderId);
        appendMessage(m, { mine: m.senderId === ME.id });
      }
    }
    // Séparateur toujours rendu — sert d'ancre aux syncs P2P ultérieures.
    appendSeparator(hasHistory ? "— Nouveaux messages —" : "— Début de la session —");
  } catch (e) {
    console.error("[lan-chat] get_history failed:", e);
  }
  appendSystem(`Connecté au salon « ${roomName} » (chiffré E2E). Découverte mDNS en cours.`);
}

export async function sendMessage(content) {
  if (!ME.id) return;
  const msg = {
    id: crypto.randomUUID(),
    senderName: ME.name,
    senderId: ME.id,
    content,
    timestamp: Date.now(),
  };
  appendMessage(msg, { mine: true });
  try {
    await invoke("send_message", { message: msg });
  } catch (e) {
    appendSystem(`Erreur d'envoi : ${e}`, "error");
  }
}
