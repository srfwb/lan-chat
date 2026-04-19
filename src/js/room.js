import {
  el,
  hideOverlay,
  hideRoomError,
  setInputEnabled,
  setStatus,
  showRoomError,
} from "./dom.js";
import { ME, fileBubbles, fileOffers, seenMessageIds, seenPeers, state } from "./state.js";
import { invoke } from "./tauri-api.js";
import { pollStatus } from "./status.js";
import { appendSystem, updateSyncIndicator } from "./messages.js";

export async function onRoomSubmit(e) {
  e.preventDefault();
  const code = el.roomInput.value.trim();
  if (!code) return;
  hideRoomError();
  try {
    await invoke("set_room_code", { code });
    state.hasAcceptedRoom = true;
    hideOverlay();
    // node-ready peut arriver avant cette ligne : on n'écrase pas un "ready" déjà posé.
    if (!state.isReady) {
      setStatus("Connexion au salon…", "info");
    }
    pollStatus();
  } catch (err) {
    console.error("[lan-chat] set_room_code failed:", err);
    showRoomError(err);
  }
}

export async function onChangeRoom() {
  const ok = confirm(
    "Changer de salon ? L'historique du salon actuel est conservé sur cette machine — tu pourras y revenir en retapant le même code."
  );
  if (!ok) return;
  try {
    await invoke("leave_room");
    // Le listener node-awaiting-room s'occupe du reset UI.
  } catch (err) {
    appendSystem(`Erreur leave_room : ${err}`, "error");
  }
}

export function resetUiForAwaitingRoom() {
  el.messages.innerHTML = "";
  seenMessageIds.clear();
  seenPeers.clear();
  fileOffers.clear();
  fileBubbles.clear();
  el.peersCount.textContent = "0";
  el.roomBadge.hidden = true;
  el.roomName.textContent = "—";
  state.isReady = false;
  ME.id = null;
  state.hasAcceptedRoom = false;
  el.meAvatar.style.background = "";
  setInputEnabled(false);
  state.historySeparator = null;
  state.activeSyncs = 0;
  updateSyncIndicator();
  if (el.dropOverlay) el.dropOverlay.hidden = true;
}
