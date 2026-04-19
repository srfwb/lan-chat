export const ME = {
  id: null,
  name: "…",
};

export const seenPeers = new Set();
export const seenMessageIds = new Set();
export const fileOffers = new Map();
export const fileBubbles = new Map();

/// Scalaires mutables : regroupés dans un objet pour que les mutations cross-module
/// soient visibles de tous les imports (contrairement à `export let`).
export const state = {
  isReady: false,
  pollToken: 0,
  hasAcceptedRoom: false,
  activeSyncs: 0,
  historySeparator: null,
};
