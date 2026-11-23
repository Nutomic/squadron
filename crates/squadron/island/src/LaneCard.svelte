<script>
  import Sparkline from './Sparkline.svelte';

  const STATE_LABELS = {
    queued: { label: 'queued', cls: 'chip chip-neutral' },
    creating: { label: 'creating', cls: 'chip chip-info chip-pulse' },
    running: { label: 'running', cls: 'chip chip-info chip-pulse' },
    done: { label: 'warm', cls: 'chip chip-neutral' },
    validated: { label: 'verified', cls: 'chip chip-success' },
    failed: { label: 'failed', cls: 'chip chip-error' },
    skipped: { label: 'skipped', cls: 'chip chip-neutral' }
  };

  let { lane, runId, isWinner = false, onTokei } = $props();

  let chip = $derived(STATE_LABELS[lane.state] ?? { label: lane.state, cls: 'chip chip-neutral' });
  let hasThumb = $derived(lane.screenshotState === 'done');
  let thumbUrl = $derived(lane.thumbnailDataUrl ?? `/${runId}/thumb/${lane.idx}`);
  let buildLabel = $derived(lane.buildMs == null ? null : fmtBuild(lane.buildMs));
  let valPassed = $derived(lane.valVerdict === 'pass');

  function fmtBuild(ms) {
    const mins = Math.floor(ms / 60000);
    if (mins >= 60) return `${Math.floor(mins / 60)}h ${mins % 60}m`;
    return `${mins}m`;
  }

  function fmtCost(c) {
    return c == null ? '—' : `$${c.toFixed(2)}`;
  }
</script>

<article
  class="lane-hue-{lane.idx % 6} {isWinner ? 'lane-card-winner' : ''} relative bg-base-100 border border-base-300 rounded-xl overflow-hidden transition-[border-color,transform] duration-150 hover:border-zinc-400 hover:-translate-y-0.5 flex flex-col"
>
  {#if isWinner}
    <div class="win-ribbon">
      <svg class="w-3 h-3" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
        <path d="M8 1l1.9 3.9 4.3.6-3.1 3 .7 4.3L8 10.8l-3.8 2 .7-4.3-3.1-3 4.3-.6L8 1z" />
      </svg>
      Winner
    </div>
  {/if}

  <a href="/{runId}/lane/{lane.idx}" class="block group relative bg-base-200" aria-label="Open lane {lane.idx} details">
    <figure class="aspect-[16/10] overflow-hidden">
      {#if hasThumb}
        <img
          src={thumbUrl}
          alt="lane {lane.idx} screenshot"
          loading="lazy"
          class="w-full h-full object-cover"
        />
        <div class="absolute inset-0 flex items-end justify-center pb-4 bg-base-content/0 group-hover:bg-base-content/25 transition-colors duration-150">
          <span class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 bg-white text-base-content text-xs font-medium rounded-full px-3.5 py-1.5 shadow-[0_8px_24px_rgb(0_0_0/0.08)]">Open preview</span>
        </div>
      {:else}
        <div class="w-full h-full flex flex-col items-center justify-center gap-2 text-base-content/40">
          {#if lane.screenshotState === 'rendering' || lane.screenshotState === 'queued'}
            <span class="loading loading-spinner loading-sm"></span>
            <span class="mono-num !text-[11px]">screenshot {lane.screenshotState}</span>
          {:else}
            <svg class="w-8 h-8 text-base-content/20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true">
              <rect x="3" y="4" width="18" height="16" rx="2" /><circle cx="9" cy="10" r="1.5" /><path d="M21 16l-5-5-8 9" />
            </svg>
            <span class="mono-num !text-[11px]">screenshot pending</span>
          {/if}
        </div>
      {/if}
    </figure>
  </a>

  <div class="p-4 flex flex-col gap-2.5 flex-1 border-t border-base-300">
    <div class="flex items-center justify-between gap-2">
      <span class="flex items-center gap-2 text-sm font-semibold tracking-tight">
        <span class="lane-dot"></span>Lane {lane.idx}
      </span>
      <span class={chip.cls}>{chip.label}</span>
    </div>

    <div class="mono-num text-base-content/60 truncate">{lane.model ?? 'agent not started'}</div>

    {#if lane.verdictCard}
      <p class="lane-edge text-[12px] leading-relaxed text-base-content/70 pl-2.5">{lane.verdictCard}</p>
    {/if}

    <dl class="grid grid-cols-3 gap-2 text-[12px]">
      <div>
        <dt class="label-meta !text-[10px]">Build</dt>
        <dd class="mono-num mt-0.5">{buildLabel ?? '—'}</dd>
      </div>
      <div>
        <dt class="label-meta !text-[10px]">Cost</dt>
        <dd class="mono-num mt-0.5">{fmtCost(lane.costUsd)}</dd>
      </div>
      <div>
        <dt class="label-meta !text-[10px]">Files / LOC</dt>
        <dd class="mono-num mt-0.5">{lane.files ?? '—'} / {lane.loc ?? '—'}</dd>
      </div>
    </dl>

    {#if lane.costSeries?.length > 1}
      <Sparkline series={lane.costSeries} />
    {/if}

    {#if lane.consoleTail}
      <details class="text-[12px]">
        <summary class="cursor-pointer text-base-content/55 hover:text-base-content select-none">Console</summary>
        <pre class="console-block whitespace-pre-wrap mt-1.5 max-h-32">{lane.consoleTail}</pre>
      </details>
    {/if}

    {#if lane.error}
      <p class="text-[12px] text-error/90">{lane.error}</p>
    {/if}

    <div class="card-actions items-center justify-end mt-auto pt-2 flex-wrap gap-1">
      {#if lane.files != null || lane.loc != null}
        <button class="btn btn-ghost btn-xs text-base-content/60 hover:text-base-content" onclick={() => onTokei(lane.idx)}>tokei</button>
      {/if}
      {#if lane.implUrl}
        <a class="btn btn-ghost btn-xs text-primary hover:text-primary" href={lane.implUrl} target="_blank" rel="noopener noreferrer">live</a>
      {/if}
      <a class="btn btn-ghost btn-xs text-base-content/60 hover:text-base-content" href="/{runId}/source/{lane.idx}">source</a>
      <a class="btn btn-primary btn-xs rounded-full px-3 border-0" href="/{runId}/lane/{lane.idx}">details</a>
    </div>
  </div>
</article>
