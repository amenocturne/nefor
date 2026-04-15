<script lang="ts">
  import { onMount } from 'svelte';

  interface AgentInfo {
    id: string;
    name: string;
    cwd: string;
    status: string;
  }

  let {
    agents,
    onselect,
    onclose,
  }: {
    agents: AgentInfo[];
    onselect: (id: string) => void;
    onclose: () => void;
  } = $props();

  let query = $state('');
  let selectedIndex = $state(0);
  let inputEl: HTMLInputElement;

  let filtered = $derived(
    query.trim() === ''
      ? agents
      : agents.filter((a) => {
          const q = query.toLowerCase();
          return a.name.toLowerCase().includes(q) || a.cwd.toLowerCase().includes(q);
        })
  );

  $effect(() => {
    // Reset selection when filter changes
    filtered;
    selectedIndex = 0;
  });

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (filtered.length > 0) {
        selectedIndex = (selectedIndex + 1) % filtered.length;
        scrollIntoView();
      }
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (filtered.length > 0) {
        selectedIndex = (selectedIndex - 1 + filtered.length) % filtered.length;
        scrollIntoView();
      }
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (filtered.length > 0 && filtered[selectedIndex]) {
        onselect(filtered[selectedIndex].id);
      }
    } else if (e.key === 'Escape') {
      e.preventDefault();
      onclose();
    }
  }

  function scrollIntoView() {
    requestAnimationFrame(() => {
      const el = document.querySelector('.spotlight-item.active');
      el?.scrollIntoView({ block: 'nearest' });
    });
  }

  onMount(() => {
    inputEl?.focus();
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onclick={onclose}>
  <div class="spotlight" onclick={(e) => e.stopPropagation()}>
    <input
      bind:this={inputEl}
      bind:value={query}
      class="spotlight-input"
      placeholder="Switch to agent..."
      onkeydown={handleKeydown}
    />
    <div class="spotlight-results">
      {#if filtered.length === 0}
        <div class="spotlight-empty">No matching agents</div>
      {/if}
      {#each filtered as agent, i (agent.id)}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="spotlight-item"
          class:active={i === selectedIndex}
          onclick={() => onselect(agent.id)}
          onmouseenter={() => selectedIndex = i}
        >
          <span
            class="status-dot"
            class:running={agent.status === 'running'}
            class:exited={agent.status !== 'running'}
          ></span>
          <div class="spotlight-item-info">
            <span class="spotlight-name">{agent.name}</span>
            <span class="spotlight-cwd">{agent.cwd}</span>
          </div>
        </div>
      {/each}
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
    justify-content: center;
    align-items: flex-start;
    padding-top: 20vh;
    z-index: 300;
  }

  .spotlight {
    width: 500px;
    background: #151515;
    border: 1px solid #2a2a2a;
    border-radius: 10px;
    overflow: hidden;
    box-shadow: 0 16px 48px rgba(0, 0, 0, 0.5);
  }

  .spotlight-input {
    width: 100%;
    padding: 14px 18px;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--border-primary);
    color: var(--text-primary);
    font-size: 16px;
    font-family: inherit;
    outline: none;
  }

  .spotlight-input::placeholder {
    color: #555555;
  }

  .spotlight-results {
    max-height: 320px;
    overflow-y: auto;
    padding: 4px 0;
  }

  .spotlight-empty {
    padding: 16px 18px;
    color: #555555;
    font-size: 13px;
    text-align: center;
  }

  .spotlight-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 18px;
    cursor: pointer;
    transition: background 0.05s;
  }

  .spotlight-item.active {
    background: #1e1e1e;
  }

  .status-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .status-dot.running {
    background: var(--accent);
  }

  .status-dot.exited {
    background: #555555;
  }

  .spotlight-item-info {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }

  .spotlight-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .spotlight-cwd {
    font-size: 12px;
    color: #666666;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
