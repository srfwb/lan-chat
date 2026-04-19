const tauriCore = window.__TAURI__ && window.__TAURI__.core;
const tauriEvent = window.__TAURI__ && window.__TAURI__.event;
const tauriDialog = window.__TAURI__ && window.__TAURI__.dialog;
const tauriShell = window.__TAURI__ && window.__TAURI__.shell;

if (!tauriCore || !tauriEvent) {
  console.error(
    "[lan-chat] window.__TAURI__ indisponible — l'app doit être lancée via `npm run tauri dev`."
  );
}

export const invoke = tauriCore
  ? tauriCore.invoke
  : async () => {
      throw new Error("Tauri indisponible");
    };

export const listen = tauriEvent ? tauriEvent.listen : async () => () => {};
export const openFileDialog = tauriDialog ? tauriDialog.open : null;
export const openWithSystem = tauriShell ? tauriShell.open : null;
