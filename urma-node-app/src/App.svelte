<script lang="ts">
  import Advanced from "./Advanced.svelte";
  import Controls from "./Controls.svelte";
  import Home from "./Home.svelte";
  import { layout } from "./layout.svelte";
  import { loading, type Phase } from "./progress";
  import { loadStatus, type Failure, type Overview as Snapshot } from "./status";

  const pollMs = 2000;
  const backoffMaxMs = 16000;

  let snapshot = $state<Snapshot | null>(null);
  let failure = $state<Failure | null>(null);

  $effect(() => {
    let timer: ReturnType<typeof setTimeout>;
    let delay = pollMs;
    let live = true;
    const poll = () =>
      loadStatus().then(
        (loaded) => {
          snapshot = loaded;
          failure = null;
          delay = pollMs;
        },
        (error: Failure) => {
          snapshot = null;
          failure = error;
          delay = Math.min(backoffMaxMs, delay * 2);
        },
      ).finally(() => {
        if (live) timer = setTimeout(poll, failure ? delay : pollMs);
      });
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  });

  const phase: Phase = $derived(failure ? "failed" : snapshot ? loading(snapshot.status) : "connecting");
</script>

<div class="shell" class:simple={layout.mode === "simple"}>
  {#if layout.mode === "simple"}
    <Home {phase} {snapshot} {failure} />
    <footer><Controls /></footer>
  {:else}
    <Advanced {phase} {snapshot} {failure} />
  {/if}
</div>
