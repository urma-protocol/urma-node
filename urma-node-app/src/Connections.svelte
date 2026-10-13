<script lang="ts">
  import { loadBook, loadOmissions, type BookEntry, type Failure, type Omission, type ServiceStatus } from "./status";
  import { ago, num, short } from "./format";

  let { status, endpoint }: { status: ServiceStatus; endpoint: string } = $props();

  const listMs = 10000;

  const archive = $derived(status.archive);
  const overlay = $derived(status.overlay);
  const on = $derived(overlay !== null);

  let book = $state<BookEntry[] | null>(null);
  let omissions = $state<Omission[] | null>(null);
  let listError = $state<string | null>(null);

  $effect(() => {
    if (!on) return;
    const load = () =>
      Promise.all([loadBook(), loadOmissions()]).then(
        ([entries, rows]) => {
          book = entries;
          omissions = rows.reverse();
          listError = null;
        },
        (failure: Failure) => (listError = failure.message),
      );
    load();
    const timer = setInterval(load, listMs);
    return () => clearInterval(timer);
  });

  const now = () => Date.now() / 1000;
</script>

<main>
  <section class="wide">
    <h2>Node API</h2>
    <dl>
      <dt>endpoint</dt>
      <dd>{endpoint}</dd>
      <dt>api</dt>
      <dd>v{archive.api_version}</dd>
      <dt>capabilities</dt>
      <dd class="muted">{status.capabilities.join(" · ")}</dd>
    </dl>
  </section>

  <section class="wide">
    <h2>Peer sync</h2>
    {#if overlay}
      <dl>
        <dt>synced through</dt>
        <dd>
          {#if archive.peer_synced_through}
            <b>#{num(archive.peer_synced_through.height)}</b>
            <code>{short(archive.peer_synced_through.hash)}</code>
          {:else}<span class="muted">none</span>{/if}
        </dd>
        <dt>admitted</dt>
        <dd>{num(overlay.admitted)} <span class="muted">this session</span></dd>
        <dt>rejected</dt>
        <dd class:bad={overlay.rejected}>{num(overlay.rejected)}</dd>
        <dt>unaudited</dt>
        <dd>{num(archive.unaudited_records)} <span class="muted">records awaiting full-block audit</span></dd>
        <dt>omissions</dt>
        <dd class:warn={archive.peer_omissions}>{num(archive.peer_omissions)} <span class="muted">found by audit</span></dd>
      </dl>
    {:else}
      <p class="muted">Peer network off.</p>
    {/if}
  </section>

  {#if overlay}
    <section class="wide">
      <h2>URMA peers <small>{overlay.connected} connected · {overlay.peer_book} known</small></h2>
      {#if overlay.peers.length}
        <dl>
          {#each overlay.peers as peer}
            <dt><code>{peer.label}</code></dt>
            <dd>
              {peer.synced_through === null ? "not synced" : `#${num(peer.synced_through)}`}
              <span class="muted">cursor {num(peer.cursor)}</span>
              {#if peer.last_error}<span class="bad">✗ {peer.last_error}</span>{/if}
              {#if peer.last_error_at}<span class="muted">{ago(peer.last_error_at)}</span>{/if}
              {#if peer.backoff_until && peer.backoff_until > now()}<span class="warn">backing off</span>{/if}
              {#if peer.last_progress_at}<span class="muted">progress {ago(peer.last_progress_at)}</span>{/if}
            </dd>
          {/each}
        </dl>
      {:else}
        <p class="muted">No peers yet.</p>
      {/if}
      <dl class="identity">
        <dt>listen</dt>
        <dd>
          {#each overlay.listen as address}
            <code>{address}</code>
          {:else}
            outbound only
          {/each}
        </dd>
        {#if overlay.announce.length}
          <dt>announced</dt>
          <dd>
            {#each overlay.announce as address}
              <code>{address}</code>
            {/each}
          </dd>
        {/if}
        <dt>peer id</dt>
        <dd><code>{overlay.peer_id}</code></dd>
      </dl>
    </section>

    <section class="wide">
      <h2>Discovery</h2>
      {#if overlay.discovery}
        {@const d = overlay.discovery}
        <dl>
          <dt>peer book</dt>
          <dd>{num(d.book)} <span class="muted">{num(d.seeds)} seeds · {num(d.gossip)} via gossip</span></dd>
          <dt>reached</dt>
          <dd>{num(d.reached)} <span class="muted">last dial · {num(d.reached_ever)} ever</span></dd>
          <dt>seed round</dt>
          <dd>{#if d.last_seed_round_error}<span class="bad">✗ {d.last_seed_round_error}</span>{:else}<span class="ok">ok</span>{/if}</dd>
          <dt>gossip</dt>
          <dd>{#if d.last_gossip_error}<span class="bad">✗ {d.last_gossip_error}</span>{:else}<span class="ok">ok</span>{/if}</dd>
        </dl>
      {:else}
        <p class="muted">Discovery off.</p>
      {/if}
    </section>

    <section class="wide">
      <h2>Peer book <small>{book ? `${num(book.length)} entries` : ""}</small></h2>
      {#if listError}
        <p class="bad">✗ {listError}</p>
      {:else if book?.length}
        <dl>
          {#each book as entry}
            <dt><code>{entry.peer_id ? short(entry.peer_id) : "unverified"}</code></dt>
            <dd>
              <span class="muted">{entry.origin}</span>
              {entry.last_success ? `ok ${ago(entry.last_success)}` : "never reached"}
              {#if entry.failures}<span class="warn">{num(entry.failures)} failures</span>{/if}
              {#if entry.banned_until && entry.banned_until > now()}<span class="bad">banned</span>{/if}
              <code>{entry.addrs.join(" ")}</code>
            </dd>
          {/each}
        </dl>
      {:else if book}
        <p class="muted">Empty.</p>
      {/if}
    </section>

    <section class="wide">
      <h2>Omissions <small>records peers left out, newest first</small></h2>
      {#if omissions?.length}
        <dl>
          {#each omissions as row}
            <dt>#{num(row.block.height)}:{row.position}</dt>
            <dd>
              <code>{short(row.txid)}</code>
              <span class="muted">by {row.peer}</span>
            </dd>
          {/each}
        </dl>
      {:else if omissions}
        <p class="muted">None found.</p>
      {/if}
    </section>
  {/if}

  <section class="wide">
    <h2>Commit sources <small>try order</small></h2>
    {#if status.commit_sources.length}
      <dl>
        {#each status.commit_sources as source, i}
          <dt>{i + 1}. {source.label}</dt>
          <dd>
            {#if source.last_error}<span class="bad">✗ {source.last_error}</span>{:else}<span class="ok">ok</span>{/if}
            {#if source.last_error_at}<span class="muted">{ago(source.last_error_at)}</span>{/if}
          </dd>
        {/each}
      </dl>
    {:else}
      <p class="muted">None configured.</p>
    {/if}
  </section>
</main>
