import { listen } from "./tauri-api.js";
import { ME, state, fileOffers } from "./state.js";
import { registerPeer } from "./peers.js";
import {
  appendMessage,
  insertHistoryMessage,
  updateSyncIndicator,
} from "./messages.js";
import {
  appendFileBubble,
  sendFile,
  updateFileBubbleCompleted,
  updateFileBubbleError,
  updateFileBubbleProgress,
} from "./files.js";
import { applyStatus } from "./status.js";
import { resetUiForAwaitingRoom } from "./room.js";
import { el } from "./dom.js";
import { pollStatus } from "./status.js";

export function registerTauriListeners() {
  listen("chat-message", (event) => {
    const msg = event.payload;
    registerPeer(msg.senderId);
    if (msg.senderId === ME.id) return; // écho de notre propre publish
    appendMessage(msg, { mine: false });
  });

  listen("history-message", (event) => {
    const msg = event.payload;
    registerPeer(msg.senderId);
    insertHistoryMessage(msg, { mine: msg.senderId === ME.id });
  });

  listen("history-sync-start", () => {
    state.activeSyncs++;
    updateSyncIndicator();
  });

  listen("history-sync-end", () => {
    state.activeSyncs = Math.max(0, state.activeSyncs - 1);
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
    pollStatus();
  });

  listen("node-error", (event) => {
    applyStatus({ kind: "error", message: event.payload });
  });

  listen("file-offer", (event) => {
    const offer = event.payload;
    if (!offer || !offer.id) return;
    registerPeer(offer.senderId);
    if (offer.senderId === ME.id) return; // écho — bulle locale déjà affichée
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

  // Drag & drop Tauri v2 : enter / over / drop / leave.
  listen("tauri://drag-enter", () => {
    if (!state.isReady) return;
    if (el.dropOverlay) el.dropOverlay.hidden = false;
  });
  listen("tauri://drag-over", () => {
    if (!state.isReady) return;
    if (el.dropOverlay) el.dropOverlay.hidden = false;
  });
  listen("tauri://drag-leave", () => {
    if (el.dropOverlay) el.dropOverlay.hidden = true;
  });
  listen("tauri://drag-drop", async (event) => {
    if (el.dropOverlay) el.dropOverlay.hidden = true;
    if (!state.isReady) return;
    const paths = event.payload?.paths;
    if (!Array.isArray(paths) || paths.length === 0) return;
    for (const p of paths) {
      await sendFile(p);
    }
  });
}
