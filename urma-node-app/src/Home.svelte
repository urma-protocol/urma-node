<script lang="ts">
  import Logo from "./Logo.svelte";
  import type { Failure, Overview } from "./status";
  import { bar, receiving, type Phase } from "./progress";
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

  const LEGEND = {
    chain: [
      { key: "chain", name: "Syncing chain" },
      { key: "pending", name: "Not loaded" },
    ],
    records: [
      { key: "verified", name: "Verified" },
      { key: "from-peers", name: "From peers" },
      { key: "pending", name: "Not loaded" },
    ],
  };
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
      <p class="legend">
        {#each LEGEND[progress.state] as item (item.key)}
          <span><i class="swatch {item.key}"></i>{item.name}</span>
        {/each}
      </p>
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
