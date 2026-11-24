<script>
  import LaneCard from './LaneCard.svelte';

  let { runId, eventsUrl, csrfToken } = $props();

  let snap = $state(null);
  let conn = $state('connecting');

  $effect(() => {
    const es = new EventSource(eventsUrl);
    es.onmessage = (e) => {
      try {
        snap = JSON.parse(e.data);
        conn = 'live';
      } catch {
        conn = 'error';
      }
    };
    es.onerror = () => {
      conn = 'reconnecting';
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
  let runStatus = $derived(snap?.run?.status ?? null);
  let runStatusChip = $derived(runStatus ? (STATUS_CHIPS[runStatus] ?? 'chip-neutral') : null);
  // A partial run (F27: some lanes validated, some failed) still has a winner
  // among the survivors — it renders the verdict card and is promotable.
  let hasWinner = $derived(
    runStatus === 'judged' || runStatus === 'partial' || runStatus === 'promoted'
  );
  let promotable = $derived(runStatus === 'judged' || runStatus === 'partial');
  let winner = $derived(hasWinner ? snap?.run?.winnerLane : null);
  let verdict = $derived(snap?.run?.judgeVerdict);
  let briefFulfillment = $derived(snap?.run?.judgeBriefFulfillment);
  let totalCost = $derived(
    snap?.lanes?.reduce((acc, l) => acc + (l.costUsd ?? 0), 0) ?? null
  );
  let runningLanes = $derived(snap?.lanes?.filter((l) => l.state === 'running' || l.state === 'creating' || l.state === 'queued').length ?? 0);

  async function openTokei(idx) {
    const dialog = document.getElementById('tokei-dialog');
    const box = dialog?.querySelector('.modal-box');
    if (!dialog || !box) return;
    box.innerHTML =
      '<div class="p-10 flex justify-center"><span class="loading loading-spinner text-base-content/40"></span></div>';
    dialog.showModal();
    try {
      const res = await fetch(`/${runId}/lane/${idx}/tokei`);
      const html = await res.text();
      box.innerHTML = renderTokei(idx, html);
      box.querySelector('[data-tokei-close]')?.addEventListener('click', () => dialog.close());
    } catch {
      box.innerHTML =
        '<div class="p-5 text-sm text-error">Failed to load tokei output.</div>';
    }
  }

  /// The tokei endpoint returns the raw JSON in a <pre>; parse it into
  /// label/value rows. Falls back to the raw output when parsing fails.
  function renderTokei(idx, html) {
    let rows = null;
    try {
      const doc = new DOMParser().parseFromString(html, 'text/html');
      const raw = doc.querySelector('pre')?.textContent ?? '';
      // tokei JSON may contain Rust-style numeric separators (44_320)
      const json = JSON.parse(raw.replace(/(\d)_(\d)/g, '$1$2'));
      const t = json.Total ?? {};
      if (t.files != null || t.code != null) {
        rows = [
          ['Files', t.files],
          ['Lines of code', t.code],
          ['Bytes', t.bytes],
        ];
      }
    } catch {
      rows = null;
    }
    const close =
      '<div class="px-5 py-3 border-t border-base-300 flex justify-end">' +
      '<button type="button" data-tokei-close class="btn btn-ghost btn-xs rounded-full">Close</button></div>';
    if (!rows) {
      return '<div class="p-5">' + html + '</div>' + close;
    }
    const fmtBytes = (b) =>
      b == null ? '—' : b >= 1024 ? `${(b / 1024).toFixed(1)} KB` : `${b} B`;
    const body = rows
      .map(
        ([label, v]) =>
          `<div class="flex items-center justify-between gap-6 py-2 border-b border-base-300 last:border-b-0">
            <span class="label-meta">${label}</span>
            <span class="mono-num">${label === 'Bytes' ? fmtBytes(v) : v == null ? '—' : Number(v).toLocaleString('en-US')}</span>
          </div>`
      )
      .join('');
    return (
      `<div class="p-5">
        <div class="flex items-center justify-between gap-3 mb-3">
          <h3 class="text-[15px] font-semibold tracking-tight flex items-center gap-2.5">
            <span class="lane-dot lane-hue-${idx % 6}"></span>Code size — lane ${idx}
          </h3>
          <span class="mono-num text-base-content/40">tokei</span>
        </div>
        <div class="px-1">${body}</div>
      </div>` + close
    );
  }
</script>

<div class="flex flex-wrap items-center gap-x-4 gap-y-2 mb-6">
  <span class="chip {conn === 'live' ? 'chip-success' : conn === 'error' ? 'chip-error' : 'chip-neutral chip-pulse'}">{conn}</span>
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
        {#if briefFulfillment}
          <div class="mt-2.5 flex items-start gap-2.5">
            <span class="chip chip-no-dot flex-none {briefFulfillment.state === 'fulfilled' ? 'chip-success' : briefFulfillment.state === 'unfulfilled' ? 'chip-error' : 'chip-neutral'}">
              {briefFulfillment.state === 'fulfilled' ? 'brief fulfilled' : briefFulfillment.state === 'unfulfilled' ? 'brief not fulfilled' : 'brief: cannot verify from evidence'}
            </span>
            <p class="text-xs leading-relaxed text-base-content/60">{briefFulfillment.evidence}</p>
          </div>
        {/if}
      </div>
    </div>
  </div>
{/if}

{#if snap && promotable}
  <!-- CSRF double-submit token (review F3 #3): the server sets the same
       token in the squadron_csrf cookie on this page's GET. -->
  <form method="post" action="/{runId}/promote" class="mb-6">
    <input type="hidden" name="csrf_token" value={csrfToken ?? ''}>
    <button type="submit" class="btn btn-primary btn-sm rounded-full px-5 font-semibold border-0">
      Promote winner (lane {winner})
    </button>
  </form>
{/if}

{#if snap?.lanes?.length}
  <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-6 items-start">
    {#each snap.lanes as lane (lane.idx)}
      <LaneCard {lane} {runId} isWinner={lane.idx === winner} onTokei={openTokei} />
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
