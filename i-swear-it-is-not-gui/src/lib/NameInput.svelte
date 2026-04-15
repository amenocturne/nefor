<script lang="ts">
  import { onMount } from 'svelte';

  let {
    defaultName,
    onaccept,
    oncancel,
  }: {
    defaultName: string;
    onaccept: (name: string) => void;
    oncancel: () => void;
  } = $props();

  let name = $state('');
  let inputEl: HTMLInputElement;

  $effect(() => {
    name = defaultName;
  });

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      const trimmed = name.trim();
      if (trimmed) {
        onaccept(trimmed);
      }
    } else if (e.key === 'Escape') {
      e.preventDefault();
      oncancel();
    }
  }

  onMount(() => {
    inputEl?.focus();
    inputEl?.select();
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onclick={oncancel}>
  <div class="modal" onclick={(e) => e.stopPropagation()}>
    <div class="modal-header">
      <span class="modal-title">Name this agent</span>
    </div>

    <div class="modal-body">
      <input
        bind:this={inputEl}
        bind:value={name}
        class="name-input"
        placeholder="Agent name"
        onkeydown={handleKeydown}
      />
    </div>

    <div class="modal-actions">
      <button
        tabindex="-1"
        class="action-btn start"
        onclick={() => { const trimmed = name.trim(); if (trimmed) onaccept(trimmed); }}
        disabled={!name.trim()}
      >Start</button>
      <button tabindex="-1" class="action-btn cancel" onclick={oncancel}>Cancel</button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    bottom: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 250;
  }

  .modal {
    background: var(--bg-secondary);
    border: 1px solid var(--border-primary);
    border-radius: 8px;
    width: 300px;
    overflow: hidden;
    box-shadow: 0 16px 48px rgba(0, 0, 0, 0.5);
  }

  .modal-header {
    padding: 14px 16px 10px;
  }

  .modal-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .modal-body {
    padding: 0 16px 12px;
  }

  .name-input {
    width: 100%;
    padding: 10px 12px;
    background: var(--bg-primary);
    border: 1px solid var(--border-primary);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 14px;
    font-family: inherit;
    outline: none;
    box-sizing: border-box;
  }

  .name-input:focus {
    border-color: #333333;
  }

  .modal-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    padding: 10px 16px;
    border-top: 1px solid var(--border-secondary);
  }

  .action-btn {
    padding: 7px 16px;
    border: 1px solid var(--border-primary);
    border-radius: 4px;
    font-size: 13px;
    font-family: inherit;
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
  }

  .cancel {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }

  .cancel:hover {
    background: var(--btn-hover);
    color: var(--text-primary);
  }

  .start {
    background: var(--accent-bg);
    border-color: var(--accent-border);
    color: var(--accent);
  }

  .start:hover {
    background: var(--accent-hover);
  }

  .start:disabled {
    opacity: 0.4;
    cursor: default;
  }
</style>
