<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';

  let {
    cwd,
    oninstall,
    onskip,
    oncancel,
  }: {
    cwd: string;
    oninstall: () => void;
    onskip: () => void;
    oncancel: () => void;
  } = $props();

  let dontAsk = $state(false);
  let installing = $state(false);

  async function handleInstall() {
    installing = true;
    try {
      await invoke('install_pi_config', { cwd });
      if (dontAsk) {
        await saveDontAskPref();
      }
      oninstall();
    } catch (e) {
      console.error('Install failed:', e);
      installing = false;
    }
  }

  async function handleSkip() {
    if (dontAsk) {
      await saveDontAskPref();
    }
    onskip();
  }

  async function saveDontAskPref() {
    try {
      const prefs = await invoke<any>('get_preferences');
      prefs.askToInstallPi = false;
      await invoke('save_preferences', { prefs });
    } catch {}
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onclick={oncancel}>
  <div class="modal" onclick={(e) => e.stopPropagation()}>
    <div class="modal-header">
      <span class="modal-title">No Configuration Found</span>
    </div>

    <div class="modal-body">
      <p class="message">No Pi configuration found in this directory.</p>
      <p class="path">{cwd}</p>
      <p class="sub">Would you like to install the default Nefor configuration?</p>

      <!-- svelte-ignore a11y_label_has_associated_control -->
      <label class="checkbox-row">
        <input type="checkbox" bind:checked={dontAsk} />
        <span>Don't ask again</span>
      </label>
    </div>

    <div class="modal-actions">
      <button tabindex="-1" class="action-btn install" onclick={handleInstall} disabled={installing}>
        {installing ? 'Installing...' : 'Install & Open'}
      </button>
      <button tabindex="-1" class="action-btn skip" onclick={handleSkip}>Open Without Config</button>
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
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 200;
  }

  .modal {
    background: var(--bg-secondary);
    border: 1px solid var(--border-primary);
    border-radius: 8px;
    width: 440px;
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

  .path {
    font-size: 12px;
    color: var(--text-dimmed);
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    word-break: break-all;
  }

  .sub {
    font-size: 13px;
    color: var(--text-secondary);
    margin-top: 4px;
  }

  .checkbox-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
    font-size: 12px;
    color: var(--text-secondary);
    cursor: pointer;
  }

  .checkbox-row input[type="checkbox"] {
    accent-color: var(--accent);
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

  .skip {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .skip:hover {
    background: var(--btn-hover);
  }

  .install {
    background: var(--accent-bg);
    border-color: var(--accent-border);
    color: var(--accent);
  }

  .install:hover {
    background: var(--accent-hover);
  }

  .install:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
