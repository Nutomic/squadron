<script>
  import LaneCard from './LaneCard.svelte';

  let { runId, eventsUrl, snapshotUrl, initialStatus } = $props();

  // Terminal runs have no live hub to subscribe to (R3 leak fix), so opening
  // an EventSource would only loop through reconnect forever.
  const TERMINAL_STATUSES = ['judged', 'partial', 'failed', 'promoted'];
  let settled = $derived(TERMINAL_STATUSES.includes(initialStatus));

  let snap = $state(null);
  let conn = $state('connecting');

  $effect(() => {
    if (settled) {
      conn = 'final';
      let cancelled = false;
      // A terminal run has no SSE stream, but its screenshot states can still
      // move AFTER this fetch: viewing the gallery re-adopts failed/pending
      // renders (bounded, once per lane per hour). Poll briefly while any lane
      // screenshot is unsettled so the card reflects the settled state without
      // a reload; the poll stops on its own once every lane is done/failed.
      const POLL_MS = 10_000;
      const MAX_POLLS = 15;
      const fetchSnapshot = () =>
        fetch(snapshotUrl ?? `/api/runs/${runId}`)
          .then((res) => (res.ok ? res.json() : null))
          .then((data) => {
            if (cancelled) return;
            snap = data;
            return data;
          })
          .catch(() => {
            if (!cancelled) conn = 'error';
            return null;
          });
      const poll = async (n) => {
        const data = await fetchSnapshot();
        if (cancelled || !data) return;
        const unsettled = (data.lanes ?? []).some(
          (l) => l.implUrl && !['done', 'failed'].includes(l.screenshotState ?? '')
        );
        if (unsettled && n < MAX_POLLS) setTimeout(() => void poll(n + 1), POLL_MS);
      };
      void poll(0);
      return () => {
        cancelled = true;
      };
    }
    const es = new EventSource(eventsUrl);
    es.onmessage = (e) => {
      try {
        snap = JSON.parse(e.data);
        if (TERMINAL_STATUSES.includes(snap?.run?.status)) {
          conn = 'final';
          es.close();
        } else {
          conn = 'live';
        }
      } catch {
        conn = 'error';
      }
    };
    es.onerror = () => {
      if (!TERMINAL_STATUSES.includes(snap?.run?.status)) conn = 'reconnecting';
    };
    return () => es.close();
  });

  // F27: run-status chip classes. Unknown statuses fall back to the neutral
  // chip (no breakage when the server learns a new value first).
  const STATUS_CHIPS = {
    judged: 'chip-success',
    partial: 'chip-warning',
    promoted: 'chip-winner',
    running: 'chip-info chip-pulse',
    queued: 'chip-neutral chip-pulse',
    failed: 'chip-error'
  };
  let runStatus = $derived(snap?.run?.status ?? initialStatus ?? null);
  let runStatusChip = $derived(runStatus ? (STATUS_CHIPS[runStatus] ?? 'chip-neutral') : null);
  // A partial run (F27: some lanes validated, some failed) still has a winner
  // among the survivors — it renders the verdict card. Legacy `promoted` runs
  // (set by the removed promote-winner feature) keep their winner ribbon.
  let hasWinner = $derived(
    runStatus === 'judged' || runStatus === 'partial' || runStatus === 'promoted'
  );
  let winner = $derived(hasWinner ? snap?.run?.winnerLane : null);
  let verdict = $derived(snap?.run?.judgeVerdict);
  let caseFulfillment = $derived(snap?.run?.judgeCaseFulfillment);
  let totalCost = $derived(
    snap?.lanes?.reduce((acc, l) => acc + (l.costUsd ?? 0), 0) ?? null
  );
  let runningLanes = $derived(snap?.lanes?.filter((l) => l.state === 'running' || l.state === 'creating' || l.state === 'queued').length ?? 0);

</script>

<div class="flex flex-wrap items-center gap-x-4 gap-y-2 mb-6">
  <span class="chip {conn === 'live' ? 'chip-success' : conn === 'final' ? 'chip-success chip-no-dot' : conn === 'error' ? 'chip-error' : 'chip-neutral chip-pulse'}">{conn}</span>
  {#if runStatus && runStatusChip}
    <span class="chip {runStatusChip}">{runStatus}</span>
  {/if}
  <span class="text-[13px] text-base-content/55">lanes: <span class="mono-num">{snap?.lanes?.length ?? '—'}</span></span>
  {#if runningLanes > 0}
    <span class="flex items-center gap-2 text-[13px] text-base-content/55">
      <span class="progress-thin w-24" aria-hidden="true"></span>
      {runningLanes} in progress
    </span>
  {/if}
  {#if totalCost != null}
    <span class="text-[13px] text-base-content/55">total cost: <span class="mono-num">${totalCost.toFixed(2)}</span></span>
  {/if}
</div>

{#if verdict}
  <div class="card bg-base-100 border border-base-300 rounded-xl mb-6">
    <div class="card-body p-5 flex-row items-start gap-4">
      <span class="chip chip-info chip-no-dot flex-none mt-0.5">Judge</span>
      <div class="min-w-0 flex-1">
        <p class="text-sm leading-relaxed text-base-content/80">{verdict}</p>
        {#if caseFulfillment}
          <div class="mt-2.5 flex items-start gap-2.5">
            <span class="chip chip-no-dot flex-none {caseFulfillment.state === 'fulfilled' ? 'chip-success' : caseFulfillment.state === 'unfulfilled' ? 'chip-error' : 'chip-neutral'}">
              {caseFulfillment.state === 'fulfilled' ? 'case fulfilled' : caseFulfillment.state === 'unfulfilled' ? 'case not fulfilled' : 'case: cannot verify from evidence'}
            </span>
            <p class="text-xs leading-relaxed text-base-content/60">{caseFulfillment.evidence}</p>
          </div>
        {/if}
      </div>
    </div>
  </div>
{/if}

{#if snap?.lanes?.length}
  <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-6 items-start">
    {#each snap.lanes as lane (lane.idx)}
      <LaneCard {lane} {runId} isWinner={lane.idx === winner} />
    {/each}
  </div>
{:else if snap}
  <div class="flex flex-col items-center gap-3 text-center py-16">
    <div class="browser-frame w-56 pointer-events-none">
      <div class="browser-frame-bar !py-1.5">
        <span class="browser-dot bg-[#E4E4E7]"></span><span class="browser-dot bg-[#E4E4E7]"></span><span class="browser-dot bg-[#E4E4E7]"></span>
        <span class="browser-url !border-dashed !py-1">https://</span>
      </div>
      <div class="h-20 border border-dashed border-base-300 m-2 rounded"></div>
    </div>
    <p class="text-base font-medium text-base-content">No lanes in this run</p>
    <p class="text-sm text-base-content/50">The judge has nothing to score yet.</p>
  </div>
{:else}
  <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-6" aria-hidden="true">
    {#each [0, 1, 2] as i (i)}
      <div class="card bg-base-100 border border-base-300 rounded-xl p-0 overflow-hidden">
        <div class="skeleton aspect-[16/10] rounded-none bg-base-200"></div>
        <div class="p-4 flex flex-col gap-3">
          <div class="flex items-center justify-between">
            <div class="skeleton h-4 w-32 bg-base-200"></div>
            <div class="skeleton h-5 w-16 rounded-full bg-base-200"></div>
          </div>
          <div class="skeleton h-3 w-24 bg-base-200"></div>
          <div class="skeleton h-3 w-full bg-base-200"></div>
          <div class="skeleton h-3 w-2/3 bg-base-200"></div>
        </div>
      </div>
    {/each}
  </div>
{/if}
