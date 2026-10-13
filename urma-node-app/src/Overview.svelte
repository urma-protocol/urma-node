<script lang="ts">
  import { pruneCache, type Failure, type ServiceStatus } from "./status";
  import { ago, mib, num, short } from "./format";
  import { audit, issues } from "./progress";

  let { status }: { status: ServiceStatus } = $props();

  const archive = $derived(status.archive);
  const tip = $derived(archive.selected_tip);
  const indexed = $derived(archive.indexed_through);
  const lag = $derived(tip && indexed ? tip.height - indexed.height : null);
  const progress = $derived(audit(status).fraction);
  const cache = $derived(status.last_cache_maintenance);
  const errors = $derived(issues(status));
  const failing = $derived(errors.filter((issue) => issue.error));
  const clean = $derived(errors.filter((issue) => !issue.error));

  let pruning = $state(false);
  let pruned = $state<string | null>(null);

  const prune = () => {
    pruning = true;
    pruneCache().then(
      (receipt) => (pruned = `removed ${num(receipt.records_removed)} records`),
      (failure: Failure) => (pruned = failure.message),
    ).finally(() => (pruning = false));
  };
</script>

<main>
  <section class="chain">
    <h2>Chain</h2>
    <dl>
      <dt>selected tip</dt>
      <dd>
        {#if tip}<b>#{num(tip.height)}</b> <code>{short(tip.hash)}</code>{:else}<span class="muted">none</span>{/if}
      </dd>
      <dt>peer synced</dt>
      <dd>
        {#if archive.peer_synced_through}
          <b>#{num(archive.peer_synced_through.height)}</b> <code>{short(archive.peer_synced_through.hash)}</code>        {:else}<span class="muted">none</span>{/if}
      </dd>
      <dt>audited through</dt>
      <dd>
        {#if indexed}<b>#{num(indexed.height)}</b> <code>{short(indexed.hash)}</code>{:else}<span class="muted">none</span>{/if}
        <span class="bar"><span style:width="{progress * 100}%"></span></span>
        {#if lag !== null}<span class:warn={lag > 0}>lag {num(lag)}</span>{/if}
      </dd>
      <dt>app start</dt>
      <dd>
        <b>#{num(archive.application_start.height)}</b>
        <code>{short(archive.application_start.hash)}</code>
        <span class="muted">retention {archive.retention} · gen {num(archive.generation)}</span>
      </dd>
    </dl>
    <p class="muted">
      scanned {num(archive.scanned_blocks)} blocks · stored {num(archive.stored_transactions)} tx
    </p>
  </section>

  <section>
    <h2>Dependencies</h2>
    <p class="big" class:warn={archive.unresolved_dependencies > 0}>
      {num(archive.unresolved_dependencies)}
      <small>unresolved</small>
    </p>
  </section>

  <section>
    <h2>Cache <small>as of last maintenance</small></h2>
    <dl>
      <dt>limit</dt>
      <dd>{mib(status.cache_limit_bytes)}</dd>
      {#if cache}
        <dt>last run</dt>
        <dd>
          {mib(cache.before_bytes)} → {mib(cache.remaining_bytes)}
          {#if cache.over_budget}<span class="bad">over budget</span>{:else}<span class="ok">ok</span>{/if}
        </dd>
        <dt>removed</dt>
        <dd>{num(cache.removed_records)} rec · {num(cache.removed_transactions)} tx</dd>
      {:else}
        <dt>last run</dt>
        <dd class="muted">never</dd>
      {/if}
    </dl>
    <button disabled={pruning} onclick={prune}>Prune now</button>
    {#if pruned}<span class="muted">{pruned}</span>{/if}
  </section>

  <section class="errors">
    <h2>Errors <small>last recorded</small></h2>
    {#each failing as issue}
      <p class="bad">
        <b>{issue.name}</b> ✗ {issue.error}
        {#if issue.at}<span class="muted">{ago(issue.at)}</span>{/if}
      </p>
    {/each}
    {#if clean.length}
      <p class="muted">clear: {clean.map((issue) => issue.name).join(" · ")}</p>
    {/if}
  </section>
</main>
