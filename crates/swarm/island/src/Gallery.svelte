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

  async function openTokei(idx) {
    const dialog = document.getElementById('tokei-dialog');
    const box = dialog?.querySelector('.modal-box');
    if (!dialog || !box) return;
    box.innerHTML = '<div class="py-8 text-center"><span class="loading loading-spinner"></span></div>';
    dialog.showModal();
    try {
      const res = await fetch(`/${runId}/lane/${idx}/tokei`);
      box.innerHTML = await res.text();
    } catch {
      box.innerHTML = '<div class="p-4 text-error">failed to load tokei output</div>';
    }
  }
</script>

<div class="flex flex-wrap items-center gap-3 mb-4 text-sm">
  <span class="badge badge-sm {conn === 'live' ? 'badge-success' : 'badge-warning'}">
    {conn === 'live' ? 'live' : conn}
  </span>
  <span class="text-base-content/60">lanes: {snap?.lanes?.length ?? '-'}</span>
  {#if totalCost != null}
    <span class="text-base-content/60">total cost: ${totalCost.toFixed(2)}</span>
  {/if}
</div>

{#if verdict}
  <div class="alert bg-base-300 border border-primary/30 mb-6">
    <span class="badge badge-primary">judge</span>
    <span class="text-sm text-base-content/90">{verdict}</span>
  </div>
{/if}

{#if snap && judged}
  <form method="post" action="/{runId}/promote" class="mb-6">
    <button type="submit" class="btn btn-accent btn-sm">
      promote winner (lane {winner})
    </button>
  </form>
{/if}

{#if snap?.lanes?.length}
  <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
    {#each snap.lanes as lane (lane.idx)}
      <LaneCard {lane} {runId} isWinner={lane.idx === winner} onTokei={openTokei} />
    {/each}
  </div>
{:else}
  <div class="text-center py-16 text-base-content/40">
    {#if snap}no lanes in this run{:else}waiting for run data...{/if}
  </div>
{/if}
