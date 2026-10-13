<script lang="ts">
  import Logo from "./Logo.svelte";
  import type { Failure, Overview } from "./status";
  import { bar, receiving, shares, type Phase } from "./progress";
  import { num } from "./format";

  let { phase, snapshot, failure }: { phase: Phase; snapshot: Overview | null; failure: Failure | null } = $props();

  const status = $derived(snapshot?.status);
  const progress = $derived(status ? bar(status) : null);
  const overlay = $derived(status?.overlay);

  const headline = $derived.by(() => {
    if (failure?.kind === "offline") return "Node not running";
    if (failure?.kind === "unconfigured") return "Node not configured";
    if (failure) return "Can't read the node";
    return "Connecting…";
  });

  const line = $derived.by(() => {
    if (!status || !progress) return null;
    if (!status.chain_peers) return "Connecting to Litecoin";
    if (progress.state === "chain") return "Syncing the chain";
    if (progress.verified === 1) return "Up to date";
    return progress.peers > 0 && receiving(status, Date.now() / 1000) ? "Receiving records from peers" : "Verifying";
  });

  const lonely = $derived(overlay !== null && overlay !== undefined && !overlay.connected && line !== "Up to date");

  const STATES = [
    { key: "verified", name: "Verified by this node", about: "Records this node found itself by reading every Litecoin block. Final." },
    { key: "from-peers", name: "Received from URMA peers", about: "Records copied from other URMA nodes, not yet checked by this node." },
    { key: "pending", name: "Not loaded yet", about: "Part of the chain this node hasn't reached yet." },
  ];

  const rows = $derived.by(() => {
    if (progress?.state !== "records") return [];
    const parts = [progress.verified, progress.peers, Math.max(0, 1 - progress.verified - progress.peers)];
    const percents = shares(parts);
    return STATES.map((s, i) => ({ ...s, part: parts[i], percent: percents[i] }));
  });
</script>

<section class="home">
  <div class="hero">
    <Logo {phase} />
    <p class="name">URMA Node</p>
    {#if !status}
      <h1 class:bad={failure && failure.kind !== "offline"}>{headline}</h1>
      {#if failure}<p class="sub">{failure.message}</p>{/if}
    {/if}
  </div>

  {#if status && progress}
    <div class="progress">
      <div class="track">
        {#if progress.state === "chain"}
          {#if progress.fill === null}
            <span class="chain sweep"></span>
          {:else}
            <span class="chain" style:width="{progress.fill * 100}%"></span>
          {/if}
        {:else}
          <span class="verified" style:width="{progress.verified * 100}%"></span>
          <span class="from-peers" style:width="{progress.peers * 100}%"></span>
        {/if}
      </div>
      <h1>{line}{lonely ? " · no URMA peers" : ""}</h1>
      <ul class="legend">
        {#if progress.state === "chain"}
          <li>
            <i class="swatch chain"></i>
            <span><b>Syncing the Litecoin chain</b>Learning where the chain ends before reading it.</span>
            <em></em>
          </li>
        {:else}
          {#each rows as row (row.key)}
            <li class:dim={row.part === 0}>
              <i class="swatch {row.key}"></i>
              <span><b>{row.name}</b>{row.about}</span>
              <em>{row.percent}%</em>
            </li>
          {/each}
        {/if}
      </ul>
    </div>

    {#if status.archive.peer_omissions}
      <p class="warning">
        ⚠ Audit found {num(status.archive.peer_omissions)}
        {status.archive.peer_omissions === 1 ? "record" : "records"} hidden by peers
      </p>
    {/if}

    <p class="peers">
      <span class:muted={!status.chain_peers}>
        <span class="dot" class:on={status.chain_peers}></span>Litecoin {status.chain_peers}
      </span>
      <span class="muted" aria-hidden="true">·</span>
      <span class:muted={!overlay?.connected}>
        <span class="dot" class:on={overlay?.connected}></span>URMA {overlay ? overlay.connected : "off"}
      </span>
    </p>
  {/if}
</section>
