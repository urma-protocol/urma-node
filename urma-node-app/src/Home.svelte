<script lang="ts">
  import Logo from "./Logo.svelte";
  import type { Failure, Overview } from "./status";
  import { audit, fast, type Phase } from "./progress";
  import { num, percent } from "./format";

  let { phase, snapshot, failure }: { phase: Phase; snapshot: Overview | null; failure: Failure | null } = $props();

  const status = $derived(snapshot?.status);
  const synced = $derived(status ? fast(status) : null);
  const audited = $derived(status ? audit(status) : null);
  const tip = $derived(status?.archive.selected_tip);
  const overlay = $derived(status?.overlay);
  const peerTip = $derived(status?.archive.peer_synced_through);

  const headline = $derived.by(() => {
    if (failure?.kind === "offline") return "Node not running";
    if (failure?.kind === "unconfigured") return "Node not configured";
    if (failure) return "Can't read the node";
    if (!audited) return "Connecting…";
    if (!tip) return "Waiting for chain headers";
    if (audited.complete) return "Up to date";
    return overlay && !overlay.connected ? "Synchronizing · no URMA peers" : "Synchronizing";
  });

  const shown = (sync: { fraction: number; complete: boolean } | null) =>
    !tip ? "—" : sync ? percent(sync.fraction, sync.complete) : "off";
</script>

<section class="home">
  <div class="hero">
    <Logo {phase} />
    <p class="name">URMA Node</p>
    <h1 class:bad={failure && failure.kind !== "offline"}>{headline}</h1>
    {#if failure}<p class="sub">{failure.message}</p>{/if}
  </div>

  {#if status && audited}
    <div class="progress">
      <div class="row">
        <span><i class="swatch audit"></i>Audited against chain</span>
        <b>{shown(audited)}</b>
      </div>
      <div class="row">
        <span><i class="swatch fast"></i>Cross-checked with peers</span>
        <b>{shown(synced)}{#if synced && peerTip}<small> · #{num(peerTip.height)}</small>{/if}</b>
      </div>
      <div class="bar stacked">
        <span class="fast" style:width="{(synced?.fraction ?? 0) * 100}%"></span>
        <span class="audit" style:width="{audited.fraction * 100}%"></span>
      </div>
      {#if audited.complete && synced && !synced.complete && peerTip}
        <p class="sub">peer has indexed through #{num(peerTip.height)}</p>
      {/if}
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
      <span class:muted={!overlay?.connected}>
        <span class="dot" class:on={overlay?.connected}></span>URMA {overlay ? overlay.connected : "off"}
      </span>
      <span class="muted">headers {tip ? `#${num(tip.height)}` : "—"}</span>
    </p>
  {/if}
</section>
