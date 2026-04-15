<script lang="ts">
  let {
    value = '',
    options = [],
    placeholder = '',
    onchange,
  }: {
    value?: string;
    options?: { id: string; label: string }[];
    placeholder?: string;
    onchange?: (value: string) => void;
  } = $props();

  let inputEl: HTMLInputElement;
  let open = $state(false);
  let highlightIndex = $state(-1);
  let query = $state('');

  $effect(() => {
    query = value;
  });

  let filtered = $derived(
    query
      ? options.filter(
          (o) =>
            o.id.toLowerCase().includes(query.toLowerCase()) ||
            o.label.toLowerCase().includes(query.toLowerCase())
        )
      : options
  );

  function handleInput(e: Event) {
    query = (e.target as HTMLInputElement).value;
    // Only show dropdown when actively typing and there are matches
    open = query.length > 0 && filtered.length > 0;
    highlightIndex = -1;
    onchange?.(query);
  }

  function handleBlur() {
    setTimeout(() => {
      open = false;
    }, 150);
  }

  function selectOption(opt: { id: string; label: string }) {
    query = opt.id;
    onchange?.(opt.id);
    open = false;
    inputEl?.focus();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (!open && filtered.length > 0) {
        open = true;
        highlightIndex = 0;
      } else if (open) {
        highlightIndex = Math.min(highlightIndex + 1, filtered.length - 1);
      }
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (open) {
        highlightIndex = Math.max(highlightIndex - 1, -1);
        if (highlightIndex < 0) open = false;
      }
    } else if (e.key === 'Enter') {
      if (open && filtered.length > 0) {
        e.preventDefault();
        const idx = highlightIndex >= 0 ? highlightIndex : 0;
        selectOption(filtered[idx]);
      }
    } else if (e.key === 'Escape') {
      if (open) {
        e.preventDefault();
        e.stopPropagation();
        open = false;
      }
    }
  }
</script>

<div class="combobox">
  <input
    bind:this={inputEl}
    type="text"
    class="text-input"
    value={query}
    {placeholder}
    oninput={handleInput}
    onblur={handleBlur}
    onkeydown={handleKeydown}
    autocomplete="off"
    autocorrect="off"
    spellcheck="false"
  />
  {#if open && filtered.length > 0}
    <div class="dropdown">
      {#each filtered as opt, i (opt.id)}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="option"
          class:highlighted={i === highlightIndex}
          onmousedown={() => selectOption(opt)}
        >
          <span class="option-id">{opt.id}</span>
          {#if opt.label !== opt.id}
            <span class="option-label">{opt.label}</span>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .combobox {
    position: relative;
    width: 100%;
  }

  .dropdown {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    max-height: 180px;
    overflow-y: auto;
    background: var(--bg-secondary);
    border: 1px solid var(--border-primary);
    border-radius: 0 0 4px 4px;
    border-top: none;
    z-index: 50;
    box-shadow: 0 4px 8px rgba(0, 0, 0, 0.2);
  }

  .option {
    padding: 5px 10px;
    cursor: pointer;
  }

  .option:hover,
  .option.highlighted {
    background: var(--bg-hover);
  }

  .option-id {
    font-size: 13px;
    color: var(--text-primary);
    font-family: 'JetBrainsMono NFM', monospace;
  }

  .option-label {
    font-size: 11px;
    color: var(--text-dimmed);
    margin-left: 8px;
  }
</style>
