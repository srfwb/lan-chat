import {
  el,
  hideOverlay,
  setInputEnabled,
  setStatus,
  showOverlay,
} from "./dom.js";
import { ME, state } from "./state.js";
import { invoke } from "./tauri-api.js";
import { colorFromString, shortPeerId } from "./utils.js";
import { renderHistoryThenSystem, appendSystem, updateSyncIndicator } from "./messages.js";

export function applyStatus(status) {
  if (!status || !status.kind) return false;

  if (status.kind === "awaitingRoom") {
    if (state.hasAcceptedRoom) {
      // set_room_code déjà envoyé : on ignore ce statut stale et on continue le poll.
      return false;
    }
    setStatus("En attente du code de salon…", "info");
    showOverlay();
    return true;
  }

  if (status.kind === "ready") {
    if (!state.isReady) {
      state.isReady = true;
      ME.id = status.peerId;
      el.meAvatar.style.background = colorFromString(ME.id);

      const roomName = status.roomName || "?";
      el.roomName.textContent = roomName;
      el.roomBadge.hidden = false;

      // Défense : un sync abort sans LeaveRoom laisse activeSyncs > 0 sur la prochaine session.
      state.activeSyncs = 0;
      updateSyncIndicator();

      setStatus(`Salon « ${roomName} » · ${shortPeerId(ME.id)}`, "ready");
      hideOverlay();
      setInputEnabled(true);

      renderHistoryThenSystem(roomName);
    }
    return true;
  }

  if (status.kind === "error") {
    setStatus(`Erreur : ${status.message}`, "error");
    appendSystem(`Le nœud libp2p n'a pas pu démarrer : ${status.message}`, "error");
    return true;
  }

  // "initializing" — continue à poll
  return false;
}

export async function pollStatus() {
  const MAX_ATTEMPTS = 150; // ~60 secondes à 400ms/tick
  const myToken = ++state.pollToken;
  for (let i = 0; i < MAX_ATTEMPTS; i++) {
    if (myToken !== state.pollToken) return; // un autre poll a été lancé
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
