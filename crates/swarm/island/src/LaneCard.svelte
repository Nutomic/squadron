<script>
  import Sparkline from './Sparkline.svelte';

  const STATE_LABELS = {
    queued: { label: 'queued', cls: 'badge-ghost' },
    creating: { label: 'creating', cls: 'badge-info' },
    running: { label: 'running', cls: 'badge-warning' },
    done: { label: 'warm', cls: 'badge-info' },
    validated: { label: 'verified', cls: 'badge-success' },
    failed: { label: 'failed', cls: 'badge-error' },
    skipped: { label: 'skipped', cls: 'badge-ghost' }
  };

  let { lane, runId, isWinner = false, onTokei } = $props();

  let chip = $derived(STATE_LABELS[lane.state] ?? { label: lane.state, cls: 'badge-ghost' });
  let hasThumb = $derived(lane.screenshotState === 'done');
  let thumbUrl = $derived(lane.thumbnailDataUrl ?? `/${runId}/thumb/${lane.idx}`);
  let buildLabel = $derived(
    lane.buildMs == null ? null : fmtBuild(lane.buildMs)
  );
  let valPassed = $derived(lane.valVerdict === 'pass');

  function fmtBuild(ms) {
    const mins = Math.floor(ms / 60000);
    if (mins >= 60) return `${Math.floor(mins / 60)}h ${mins % 60}m`;
    return `${mins}m`;
  }

  function fmtCost(c) {
    return c == null ? '-' : `$${c.toFixed(2)}`;
  }
</script>

<div class="card bg-base-100 border {isWinner ? 'border-accent shadow-[0_0_12px_-4px] shadow-accent' : 'border-base-content/10'} relative overflow-visible">
  {#if isWinner}
    <div class="absolute -top-2 left-3 z-10">
      <span class="badge badge-accent text-accent-content font-bold">WINNER</span>
    </div>
  {/if}
  <figure class="h-40 bg-base-300 rounded-t-lg overflow-hidden border-b border-base-content/10">
    {#if hasThumb}
      <img src={thumbUrl} alt="lane {lane.idx} screenshot" loading="lazy" class="w-full h-full object-cover" />
    {:else}
      <div class="w-full h-full flex flex-col items-center justify-center gap-2 text-base-content/40">
        {#if lane.screenshotState === 'rendering' || lane.screenshotState === 'queued'}
          <span class="loading loading-spinner"></span>
          <span class="text-xs">screenshot {lane.screenshotState}</span>
        {:else}
          <span class="text-xs">screenshot pending</span>
        {/if}
      </div>
    {/if}
  </figure>
  <div class="card-body p-4 gap-2">
    <div class="flex items-center justify-between gap-2">
      <span class="font-bold text-primary">lane {lane.idx}</span>
      <span class="badge badge-sm {chip.cls}">{chip.label}</span>
    </div>
    <div class="text-xs text-base-content/60">{lane.model ?? 'agent not started'}</div>
    {#if lane.verdictCard}
      <p class="text-xs text-accent/90 border-l-2 border-accent/50 pl-2">{lane.verdictCard}</p>
    {/if}
    <dl class="grid grid-cols-2 gap-x-2 gap-y-1 text-xs text-base-content/70">
      <dt>build</dt><dd>{buildLabel ?? '-'}</dd>
      <dt>cost</dt><dd>{fmtCost(lane.costUsd)}</dd>
      <dt>files / loc</dt>
      <dd>{lane.files ?? '-'} / {lane.loc ?? '-'}</dd>
      <dt>validator</dt>
      <dd>
        {#if lane.valVerdict}
          <span class="badge badge-xs {valPassed ? 'badge-success' : 'badge-error'}">{lane.valVerdict}</span>
        {:else}
          -
        {/if}
      </dd>
    </dl>
    {#if lane.costSeries?.length > 1}
      <Sparkline series={lane.costSeries} />
    {/if}
    {#if lane.consoleTail}
      <details class="text-xs">
        <summary class="cursor-pointer text-base-content/60">console</summary>
        <pre class="whitespace-pre-wrap mt-1 bg-base-200 rounded p-2 max-h-32 overflow-auto text-base-content/70">{lane.consoleTail}</pre>
      </details>
    {/if}
    {#if lane.error}
      <p class="text-xs text-error/80">{lane.error}</p>
    {/if}
    <div class="card-actions justify-end mt-1 flex-wrap">
      {#if lane.files != null || lane.loc != null}
        <button class="btn btn-ghost btn-xs" onclick={() => onTokei(lane.idx)}>tokei</button>
      {/if}
      {#if lane.implUrl}
        <a class="btn btn-ghost btn-xs text-primary" href={lane.implUrl} target="_blank" rel="noopener noreferrer">live</a>
      {/if}
      <a class="btn btn-ghost btn-xs" href="/{runId}/source/{lane.idx}">source</a>
      <a class="btn btn-primary btn-xs" href="/{runId}/lane/{lane.idx}">details</a>
    </div>
  </div>
</div>
