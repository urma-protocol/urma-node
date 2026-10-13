<script lang="ts">
  import Connections from "./Connections.svelte";
  import Controls from "./Controls.svelte";
  import Logo from "./Logo.svelte";
  import Overview from "./Overview.svelte";
  import { short } from "./format";
  import type { Phase } from "./progress";
  import type { Failure, Overview as Snapshot } from "./status";

  let { phase, snapshot, failure }: { phase: Phase; snapshot: Snapshot | null; failure: Failure | null } =
    $props();

  const tabs = ["Overview", "Connections", "Objects", "Wire", "Names"] as const;
  const ready = new Set(["Overview", "Connections"]);
  let tab = $state<(typeof tabs)[number]>("Overview");
</script>

<nav>
  <span class="brand"><Logo {phase} />URMA Node</span>
  <div class="tabs">
    {#each tabs as name}
      <button class:active={tab === name} disabled={!ready.has(name)} onclick={() => (tab = name)}>{name}</button>
    {/each}
  </div>
  <Controls />
</nav>

{#if snapshot}
  {@const status = snapshot.status}
  <header>
    <span>{snapshot.chain ?? `unknown chain ${short(status.archive.chain_genesis)}`}</span>
    <span>api v{status.archive.api_version}</span>
    <span>rules v{status.archive.scan_rules_version}</span>
  </header>
  {#if tab === "Overview"}
    <Overview {status} />
  {:else if tab === "Connections"}
    <Connections {status} endpoint={snapshot.endpoint} />
  {/if}
{:else if failure}
  <p class="state" class:error={failure.kind !== "offline"}>✗ {failure.message}</p>
{:else}
  <p class="state">Connecting…</p>
{/if}
