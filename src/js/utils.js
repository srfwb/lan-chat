export function colorFromString(str) {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = str.charCodeAt(i) + ((hash << 5) - hash);
  }
  const hue = Math.abs(hash) % 360;
  return `hsl(${hue}, 68%, 58%)`;
}

export function initials(name) {
  const parts = name.trim().split(/\s|-|_/).filter(Boolean);
  const letters = parts.slice(0, 2).map((p) => p[0].toUpperCase()).join("");
  return letters || "?";
}

export function formatTime(ts) {
  const d = new Date(ts);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function shortPeerId(pid) {
  if (!pid || pid.length < 16) return pid || "?";
  return `${pid.slice(0, 10)}…${pid.slice(-4)}`;
}

export function formatBytes(n) {
  if (!Number.isFinite(n) || n < 0) return "? o";
  if (n < 1024) return `${n} o`;
  const units = ["Ko", "Mo", "Go", "To"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 10 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

export function fileIconForMime(mime) {
  if (!mime) return "📄";
  if (mime.startsWith("image/")) return "🖼";
  if (mime.startsWith("video/")) return "🎬";
  if (mime.startsWith("audio/")) return "🎵";
  if (mime.includes("pdf")) return "📕";
  if (mime.includes("zip") || mime.includes("compressed") || mime.includes("tar")) return "🗜";
  if (mime.startsWith("text/") || mime.includes("json") || mime.includes("xml")) return "📝";
  return "📄";
}
