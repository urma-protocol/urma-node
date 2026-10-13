import type { ArchiveStatus, ChainPoint, ServiceStatus } from "./status";

export type Sync = { fraction: number; complete: boolean };

export type Phase = "connecting" | "failed" | Sync;

const reach = (archive: ArchiveStatus, point: ChainPoint | null): Sync => {
  const { application_start: start, selected_tip: tip } = archive;
  if (!tip || !point) return { fraction: 0, complete: false };
  const span = tip.height - start.height;
  const fraction = span > 0 ? Math.min(1, (point.height - start.height) / span) : 1;
  return { fraction, complete: point.height === tip.height };
};

export const audit = (status: ServiceStatus): Sync => {
  const audited = reach(status.archive, status.archive.indexed_through);
  return { ...audited, complete: audited.complete && status.archive.unresolved_dependencies === 0 };
};

/** Simple view bar over application_start → selected_tip: verified, then records from peers ahead of it. */
export type Bar = { state: "chain" } | { state: "records"; verified: number; peers: number };

export const bar = (status: ServiceStatus): Bar => {
  const { archive } = status;
  if (!archive.selected_tip) return { state: "chain" };
  const verified = reach(archive, archive.indexed_through).fraction;
  const peers = Math.max(0, reach(archive, archive.peer_synced_through).fraction - verified);
  return { state: "records", verified, peers };
};

/** Floored whole percents of a partition; the rounding leftover goes to the last non-empty part, so the
 * shares sum to 100 and no earlier part is ever rounded up (an incomplete "verified" never shows 100). */
export const shares = (parts: number[]): number[] => {
  const out = parts.map((p) => Math.floor(p * 100 + 1e-9));
  const last = parts.reduce((found, p, i) => (p > 0 ? i : found), -1);
  if (last >= 0) out[last] += 100 - out.reduce((a, b) => a + b, 0);
  return out;
};

/** Peers delivered inventory within this many seconds. */
const RECEIVING = 10;

export const receiving = (status: ServiceStatus, now: number) =>
  (status.overlay?.peers ?? []).some((p) => p.last_progress_at !== null && now - p.last_progress_at <= RECEIVING);

export const loading = (status: ServiceStatus): Phase => {
  const b = bar(status);
  return b.state === "chain" ? "connecting" : { fraction: b.verified, complete: b.verified === 1 };
};

export type Issue = { name: string; error: string | null; at: number | null };

export const issues = (status: ServiceStatus): Issue[] => {
  const discovery = status.overlay?.discovery;
  return [
    { name: "sync", error: status.last_sync_error, at: status.last_sync_error_at },
    { name: "dependency", error: status.last_dependency_error, at: status.last_dependency_error_at },
    { name: "inventory", error: status.last_inventory_error, at: status.last_inventory_error_at },
    { name: "discovery", error: status.last_discovery_error, at: status.last_discovery_error_at },
    {
      name: "seed round",
      error: discovery?.last_seed_round_error ?? null,
      at: discovery?.last_seed_round_error_at ?? null,
    },
    { name: "gossip", error: discovery?.last_gossip_error ?? null, at: discovery?.last_gossip_error_at ?? null },
    {
      name: "omissions",
      error: status.archive.peer_omissions ? `${status.archive.peer_omissions} records hidden by peers` : null,
      at: null,
    },
    ...(status.overlay?.peers ?? []).map((p) => ({ name: `peer ${p.label}`, error: p.last_error, at: p.last_error_at })),
    ...status.commit_sources.map((s) => ({ name: `commit ${s.label}`, error: s.last_error, at: s.last_error_at })),
    { name: "retention", error: status.last_retention_error, at: status.last_retention_error_at },
    { name: "names", error: status.last_names_error, at: status.last_names_error_at },
    { name: "cache", error: status.last_cache_error, at: status.last_cache_error_at },
  ];
};
