import { el, RING_CIRCUMFERENCE } from "./dom.js";
import { ME, fileBubbles, fileOffers } from "./state.js";
import {
  colorFromString,
  fileIconForMime,
  formatBytes,
  formatTime,
  initials,
} from "./utils.js";
import { invoke, openFileDialog, openWithSystem } from "./tauri-api.js";
import { appendSystem } from "./messages.js";

function buildFileBubbleElement(offer, { mine, state: bubbleState }) {
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

  const fileRow = document.createElement("div");
  fileRow.className = "file-bubble";
  fileRow.dataset.fileId = offer.id;
  fileRow.dataset.state = bubbleState;
  fileRow.dataset.mine = String(!!mine);
  fileRow.dataset.senderPeer = offer.senderId;

  const action = document.createElement("button");
  action.type = "button";
  action.className = "file-bubble__action";
  action.setAttribute("aria-label", "Télécharger le fichier");
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

  applyFileBubbleState(fileRow, offer, bubbleState);
  return wrapper;
}

export function appendFileBubble(offer, { mine, state }) {
  if (fileBubbles.has(offer.id)) return;
  const wrapper = buildFileBubbleElement(offer, { mine, state });
  el.messages.appendChild(wrapper);
  fileBubbles.set(offer.id, wrapper.querySelector(".file-bubble"));
  el.messages.scrollTop = el.messages.scrollHeight;
}

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

export function updateFileBubbleProgress(fileId, received, total) {
  const fileRow = fileBubbles.get(fileId);
  if (!fileRow) return;
  const pct = total > 0 ? (received / total) * 100 : 0;
  setRingPct(fileRow, pct);
  const metaEl = fileRow.querySelector(".file-bubble__meta");
  if (metaEl) {
    metaEl.textContent = `${formatBytes(received)} / ${formatBytes(total)} · ${Math.round(pct)} %`;
  }
}

export function updateFileBubbleCompleted(fileId, localPath) {
  const fileRow = fileBubbles.get(fileId);
  if (!fileRow) return;
  const offer = fileOffers.get(fileId);
  if (!offer) return;
  fileRow.dataset.localPath = localPath;
  applyFileBubbleState(fileRow, offer, "completed");
}

export function updateFileBubbleError(fileId, message) {
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

export async function sendFile(path) {
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

export async function onAttachClick() {
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
