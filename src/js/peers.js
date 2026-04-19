import { el } from "./dom.js";
import { ME, seenPeers } from "./state.js";

export function registerPeer(senderId) {
  if (!senderId || senderId === ME.id) return;
  const before = seenPeers.size;
  seenPeers.add(senderId);
  if (seenPeers.size !== before) {
    el.peersCount.textContent = String(seenPeers.size);
  }
}
