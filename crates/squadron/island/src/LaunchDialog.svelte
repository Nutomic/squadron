<script>
  // Wave C: prompt-editing dialog. The landing form's submit triggers a
  // stateless preview (POST /api/preview); the dialog shows one editable card
  // per worker case. Confirm posts the edited cases to POST /runs — the only
  // persistence point. Cancel stores nothing.
  let { csrfToken } = $props();

  const MAX_WORKERS = 8;

  let previewing = $state(false);
  let confirming = $state(false);
  let error = $state('');
  let brief = $state('');
  let cases = $state([]);
  let dialogEl = $state(null);
  let previewAbort = null;

  let atCap = $derived(cases.length >= MAX_WORKERS);

  $effect(() => {
    const form = document.getElementById('launch-form');
    if (!form) return;
    const onSubmit = (e) => {
      e.preventDefault();
      preview();
    };
    form.addEventListener('submit', onSubmit);
    return () => form.removeEventListener('submit', onSubmit);
  });

  async function preview() {
    const textarea = document.getElementById('brief');
    brief = textarea?.value?.trim() ?? '';
    if (!brief) {
      textarea?.focus();
      return;
    }
    previewAbort?.abort();
    const controller = new AbortController();
    previewAbort = controller;
    previewing = true;
    error = '';
    cases = [];
    openDialog();
    try {
      const res = await fetch('/api/preview', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ brief, csrf_token: csrfToken ?? '' }),
        signal: controller.signal
      });
      if (!res.ok) {
        error = await errorMessage(res, 'Preview failed.');
        return;
      }
      const body = await res.json();
      cases = (body.cases ?? []).slice(0, MAX_WORKERS).map((c) => ({
        label: c.label ?? '',
        impl_prompt: c.impl_prompt ?? ''
      }));
      if (cases.length === 0) {
        error = 'The planner returned no workers — try rephrasing the prompt.';
      }
    } catch (e) {
      if (e?.name === 'AbortError') return;
      error = 'Network error — the preview never reached the server.';
    } finally {
      if (previewAbort === controller) {
        previewing = false;
        previewAbort = null;
      }
    }
  }

  function openDialog() {
    dialogEl?.showModal();
  }

  function closeDialog() {
    if (previewing) {
      previewAbort?.abort();
      previewAbort = null;
      previewing = false;
    }
    error = '';
    dialogEl?.close();
  }

  async function confirm() {
    confirming = true;
    error = '';
    try {
      const res = await fetch('/runs', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          brief,
          csrf_token: csrfToken ?? '',
          cases: cases.map((c) => ({ label: c.label, impl_prompt: c.impl_prompt }))
        })
      });
      if (!res.ok) {
        error = await errorMessage(res, 'Launch failed.');
        return;
      }
      const body = await res.json();
      closeDialog();
      window.location.assign(`/${body.run_id}`);
    } catch {
      error = 'Network error — the run was not created.';
    } finally {
      confirming = false;
    }
  }

  async function errorMessage(res, fallback) {
    try {
      const body = await res.json();
      return body.error ?? fallback;
    } catch {
      return res.status === 403
        ? 'Session expired — reload the page and try again.'
        : fallback;
    }
  }

  function addWorker() {
    if (atCap) return;
    cases = [...cases, { label: '', impl_prompt: '' }];
  }

  function removeCase(i) {
    cases = cases.filter((_, idx) => idx !== i);
  }
</script>

<dialog id="launch-dialog" class="modal" bind:this={dialogEl} onclose={() => { if (previewing) { previewAbort?.abort(); previewAbort = null; previewing = false; } }}>
  <div class="modal-box bg-base-100 max-w-2xl p-0 overflow-hidden">
    {#if previewing}
      <div class="p-6 border-b border-base-300 flex items-start justify-between gap-4">
        <div>
          <p class="label-meta">Planning</p>
          <h2 class="mt-1 text-lg font-semibold tracking-tight">Planning your workers…</h2>
          <p class="mt-1 text-[13px] text-base-content/60">
            Splitting your prompt into parallel worker prompts — this takes ~10–30 seconds.
          </p>
        </div>
        <span class="loading loading-spinner loading-sm text-primary flex-none mt-1" aria-label="Planning"></span>
      </div>
      <div class="p-6 flex flex-col gap-3" aria-live="polite">
        <div class="h-4 w-2/3 rounded bg-base-200"></div>
        <div class="h-20 rounded-xl border border-base-300 bg-base-200/60"></div>
        <div class="h-20 rounded-xl border border-base-300 bg-base-200/60"></div>
      </div>
      <div class="p-6 border-t border-base-300 flex items-center justify-end gap-3">
        <button
          type="button"
          class="btn btn-ghost btn-sm rounded-full"
          onclick={closeDialog}>Cancel</button>
      </div>
      {#if error}
        <div class="px-6 pb-5">
          <p class="chip chip-error chip-no-dot !whitespace-normal">{error}</p>
        </div>
      {/if}
    {:else}
      <div class="p-6 border-b border-base-300 flex items-start justify-between gap-4">
        <div>
          <p class="label-meta">Workers</p>
          <h2 class="mt-1 text-lg font-semibold tracking-tight">Review the plan</h2>
          <p class="mt-1 text-[13px] text-base-content/60">
            Each worker is one parallel agent — it builds its own variant on an
            isolated Sprite. Edit, delete or add workers; a judge crowns the winner.
          </p>
        </div>
        <span class="chip chip-no-dot chip-neutral flex-none">{cases.length}/{MAX_WORKERS}</span>
      </div>

      <div class="max-h-[50vh] overflow-y-auto p-6 flex flex-col gap-4">
        {#each cases as c, i (i)}
          <div class="card bg-base-100 border border-base-300 rounded-xl p-4 lane-edge lane-hue-{i % 6}">
            <div class="flex items-center gap-3">
              <span class="lane-dot lane-hue-{i % 6}" aria-hidden="true"></span>
              <label class="sr-only" for="case-label-{i}">Worker {i + 1} label</label>
              <input
                id="case-label-{i}"
                class="input input-bordered input-sm flex-1 bg-base-100 border-[#E4E4E7] rounded-lg font-medium focus:border-primary focus:outline-none"
                bind:value={c.label}
                placeholder="worker {i + 1} label"
                maxlength="200"
              />
              <button
                type="button"
                class="btn btn-ghost btn-sm rounded-full text-base-content/50 hover:text-error"
                aria-label="Delete worker {i + 1}"
                onclick={() => removeCase(i)}
              >✕</button>
            </div>
            <textarea
              class="textarea textarea-bordered w-full mt-3 bg-base-100 border-[#E4E4E7] rounded-lg text-[13px] leading-relaxed focus:border-primary focus:outline-none"
              rows="4"
              aria-label="Worker {i + 1} prompt"
              bind:value={c.impl_prompt}
              placeholder="What this worker builds — a full, self-contained prompt."></textarea>
          </div>
        {/each}
      </div>

      <div class="p-6 border-t border-base-300 flex flex-wrap items-center gap-3">
        <button
          type="button"
          class="btn btn-ghost btn-sm rounded-full border border-base-300"
          onclick={addWorker}
          disabled={atCap}
        >+ Add worker</button>
        <div class="flex-1"></div>
        <button
          type="button"
          class="btn btn-ghost btn-sm rounded-full"
          onclick={closeDialog}>Cancel</button>
        <button
          type="button"
          class="btn btn-primary btn-sm rounded-full px-6 font-semibold border-0"
          onclick={confirm}
          disabled={confirming || cases.length === 0}
        >
          {#if confirming}<span class="loading loading-spinner loading-xs"></span>Launching{:else}Launch {cases.length} worker{cases.length === 1 ? '' : 's'}{/if}
        </button>
      </div>

      {#if error}
        <div class="px-6 pb-5">
          <p class="chip chip-error chip-no-dot !whitespace-normal">{error}</p>
        </div>
      {/if}
    {/if}
  </div>
  <form method="dialog" class="modal-backdrop"><button aria-label="Close dialog">close</button></form>
</dialog>
