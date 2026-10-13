import { invoke } from "@tauri-apps/api/core";

export type ChainPoint = { height: number; hash: string };

export type ArchiveStatus = {
  api_version: number;
  chain_genesis: string;
  application_start: ChainPoint;
  retention: "pruned" | "unpruned";
  generation: number;
  selected_tip: ChainPoint | null;
  indexed_through: ChainPoint | null;
  scanned_blocks: number;
  unresolved_dependencies: number;
  stored_transactions: number;
  scan_rules_version: number;
  peer_synced_through: ChainPoint | null;
  peer_omissions: number;
  unaudited_records: number;
};

export type CacheReceipt = {
  enabled: boolean;
  limit_bytes: number;
  before_bytes: number;
  remaining_bytes: number;
  removed_records: number;
  removed_transactions: number;
  over_budget: boolean;
};

export type CommitSource = { label: string; last_error: string | null; last_error_at: number | null };

export type DiscoveryStatus = {
  seeds: number;
  book: number;
  reached: number;
  reached_ever: number;
  gossip: number;
  last_seed_round_error: string | null;
  last_seed_round_error_at: number | null;
  last_gossip_error: string | null;
  last_gossip_error_at: number | null;
};

export type OverlayPeer = {
  label: string;
  cursor: number;
  synced_through: number | null;
  last_error: string | null;
  last_error_at: number | null;
  backoff_until: number | null;
  last_progress_at: number | null;
};

export type Omission = { peer: string; block: ChainPoint; position: number; txid: string; generation: number };

export type BookEntry = {
  peer_id: string | null;
  addrs: string[];
  origin: "gossip" | "seed" | "manual";
  last_success: number | null;
  failures: number;
  banned_until: number | null;
};

export type OverlayStatus = {
  peer_id: string;
  listen: string[];
  announce: string[];
  peer_book: number;
  connected: number;
  admitted: number;
  rejected: number;
  peers: OverlayPeer[];
  discovery: DiscoveryStatus | null;
};

export type ServiceStatus = {
  archive: ArchiveStatus;
  last_sync_error: string | null;
  last_sync_error_at: number | null;
  last_retention_error: string | null;
  last_retention_error_at: number | null;
  last_names_error: string | null;
  last_names_error_at: number | null;
  last_dependency_error: string | null;
  last_dependency_error_at: number | null;
  last_inventory_error: string | null;
  last_inventory_error_at: number | null;
  last_discovery_error: string | null;
  last_discovery_error_at: number | null;
  commit_sources: CommitSource[];
  cache_limit_bytes: number;
  last_cache_error: string | null;
  last_cache_error_at: number | null;
  last_cache_maintenance: CacheReceipt | null;
  overlay: OverlayStatus | null;
  chain_peers: number;
  retention_configured: boolean;
  record_sources_configured: number;
  capabilities: string[];
};

export type Overview = { endpoint: string; chain: string | null; status: ServiceStatus };

export type Failure = { kind: "unconfigured" | "config" | "offline" | "node"; message: string };

export const loadStatus = () => invoke<Overview>("node_status");

export const pruneCache = () => invoke<{ records_removed: number }>("prune_cache");

export const loadOmissions = () => invoke<Omission[]>("overlay_omissions");

export const loadBook = () => invoke<BookEntry[]>("overlay_book");
