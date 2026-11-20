<script>
  let { series = [] } = $props();

  let points = $derived.by(() => {
    if (!series || series.length < 2) return '';
    const min = Math.min(...series);
    const max = Math.max(...series);
    const span = max - min || 1;
    return series
      .map((v, i) => {
        const x = (i / (series.length - 1)) * 100;
        const y = 18 - ((v - min) / span) * 16;
        return `${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(' ');
  });

  let area = $derived.by(() => {
    if (!points) return '';
    return `0,20 ${points} 100,20`;
  });

  let total = $derived(series.length ? series[series.length - 1] : null);
</script>

{#if points}
  <div class="flex items-center gap-2 mt-0.5">
    <svg viewBox="0 0 100 20" preserveAspectRatio="none" class="w-full h-5" aria-hidden="true">
      <polygon points={area} fill="var(--lane-hue, #71717A)" opacity="0.1" />
      <polyline
        {points}
        fill="none"
        stroke="var(--lane-hue, #71717A)"
        stroke-width="1.5"
        vector-effect="non-scaling-stroke"
      />
    </svg>
    {#if total != null}
      <span class="mono-num !text-[11px] text-base-content/50 whitespace-nowrap">${total.toFixed(2)}</span>
    {/if}
  </div>
  <div class="label-meta !text-[10px] -mt-1">cost over time</div>
{/if}
