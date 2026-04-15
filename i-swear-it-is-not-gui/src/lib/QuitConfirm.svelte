<script lang="ts">
  import { onMount } from 'svelte';

  let {
    onconfirm,
    oncancel,
  }: {
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  let cancelBtn: HTMLButtonElement;

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') {
      e.preventDefault();
      onconfirm();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      oncancel();
    }
  }

  onMount(() => {
    cancelBtn?.focus();
  });
</script>

<svelte:window onkeydown={handleKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onclick={oncancel}>
  <div class="modal" onclick={(e) => e.stopPropagation()}>
    <div class="modal-header">
      <span class="modal-title">Quit Nefor?</span>
    </div>

    <div class="modal-body">
      <p class="message">All running agents will be terminated.</p>
      <p class="sub">Session persistence is not supported yet in this beta.</p>
    </div>

    <div class="modal-actions">
      <button tabindex="-1" class="action-btn quit" onclick={onconfirm}>Quit</button>
      <button bind:this={cancelBtn} tabindex="-1" class="action-btn cancel" onclick={oncancel}>Cancel</button>
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
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 400;
  }

  .modal {
    background: var(--bg-secondary);
    border: 1px solid var(--border-primary);
    border-radius: 8px;
    width: 380px;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .modal-header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border-secondary);
  }

  .modal-title {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .modal-body {
    padding: 20px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .message {
    font-size: 13px;
    color: var(--text-primary);
  }

  .sub {
    font-size: 13px;
    color: var(--text-secondary);
  }

  .modal-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    padding: 16px 20px;
    border-top: 1px solid var(--border-secondary);
  }

  .action-btn {
    padding: 8px 16px;
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

  .quit {
    background: var(--danger-bg);
    border-color: var(--danger-border);
    color: var(--danger);
  }

  .quit:hover {
    background: var(--danger-bg); opacity: 0.9;
  }
</style>
