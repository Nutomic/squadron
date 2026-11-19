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
</script>

{#if points}
  <svg viewBox="0 0 100 20" preserveAspectRatio="none" class="w-full h-5" aria-hidden="true">
    <polyline {points} fill="none" stroke="currentColor" stroke-width="1.5" class="text-accent/70" vector-effect="non-scaling-stroke" />
  </svg>
  <div class="text-[10px] text-base-content/40">cost over time</div>
{/if}
