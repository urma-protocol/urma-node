const grouped = new Intl.NumberFormat("en-US");

export const num = (n: number) => grouped.format(n).replaceAll(",", " ");

export const short = (hash: string) => `${hash.slice(0, 4)}…${hash.slice(-4)}`;

export const percent = (fraction: number, complete: boolean) =>
  complete ? "100%" : `${(Math.floor(fraction * 1000) / 10).toFixed(1)}%`;

export const mib = (bytes: number) => `${(bytes / 1048576).toFixed(1)} MiB`;

export const ago = (unix: number) => {
  const s = Math.max(0, Math.round(Date.now() / 1000 - unix));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
};
