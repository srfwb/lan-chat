export const el = {
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

/// Circonférence de l'anneau SVG : 2π × 19px de rayon.
export const RING_CIRCUMFERENCE = 2 * Math.PI * 19;

// ─── Helpers DOM simples (regroupés ici pour éviter les cycles d'imports) ───

export function setStatus(text, variant = "info") {
  el.status.textContent = text;
  el.status.dataset.variant = variant;
}

export function setInputEnabled(enabled) {
  el.input.disabled = !enabled;
  el.send.disabled = !enabled;
  if (el.attachBtn) el.attachBtn.disabled = !enabled;
  if (enabled) el.input.focus();
}

export function showOverlay() {
  el.overlay.hidden = false;
  el.roomInput.focus();
}

export function hideOverlay() {
  el.overlay.hidden = true;
  hideRoomError();
}

export function showRoomError(msg) {
  el.roomError.textContent = String(msg);
  el.roomError.hidden = false;
}

export function hideRoomError() {
  el.roomError.hidden = true;
  el.roomError.textContent = "";
}
