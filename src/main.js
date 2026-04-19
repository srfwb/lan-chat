import { el } from "./js/dom.js";
import { ME } from "./js/state.js";
import { invoke } from "./js/tauri-api.js";
import { initials } from "./js/utils.js";
import { pollStatus } from "./js/status.js";
import { onAttachClick } from "./js/files.js";
import { onChangeRoom, onRoomSubmit } from "./js/room.js";
import { sendMessage } from "./js/messages.js";
import { registerTauriListeners } from "./js/listeners.js";

registerTauriListeners();

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
