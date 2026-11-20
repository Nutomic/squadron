<script>
  import LaneCard from './LaneCard.svelte';

  let { runId, eventsUrl } = $props();

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

  let judged = $derived(snap?.run?.status === 'judged' || snap?.run?.status === 'promoted');
  let winner = $derived(judged ? snap?.run?.winnerLane : null);
  let verdict = $derived(snap?.run?.judgeVerdict);
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
      box.innerHTML = await res.text();
    } catch {
      box.innerHTML =
        '<div class="p-5 text-sm text-error">Failed to load tokei output.</div>';
    }
  }
</script>

<div class="flex flex-wrap items-center gap-x-4 gap-y-2 mb-6">
  <span class="chip {conn === 'live' ? 'chip-success' : conn === 'error' ? 'chip-error' : 'chip-neutral chip-pulse'}">{conn}</span>
  <span class="text-[13px] text-base-content/55">lanes: <span class="mono-num">{snap?.lanes?.length ?? '—'}</span></span>
  {#if runningLanes > 0}
    <div class="progress-thin w-32" aria-hidden="true"></div>
    <span class="text-[13px] text-base-content/55">{runningLanes} in progress</span>
  {/if}
  {#if totalCost != null}
    <span class="text-[13px] text-base-content/55">total cost: <span class="mono-num">${totalCost.toFixed(2)}</span></span>
  {/if}
</div>

{#if verdict}
  <div class="card bg-base-100 border border-base-300 rounded-xl mb-6">
    <div class="card-body p-5 flex-row items-start gap-4">
      <span class="chip chip-info chip-no-dot flex-none mt-0.5">Judge</span>
      <p class="text-sm leading-relaxed text-base-content/80">{verdict}</p>
    </div>
  </div>
{/if}

{#if snap && judged}
  <form method="post" action="/{runId}/promote" class="mb-6">
    <button type="submit" class="btn btn-primary btn-sm rounded-full px-5 font-semibold border-0">
      Promote winner (lane {winner})
    </button>
  </form>
{/if}

{#if snap?.lanes?.length}
  <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-6">
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
    {#each [0, 1, 2] as _i}
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
