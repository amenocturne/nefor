<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { invoke } from '@tauri-apps/api/core';

  interface AgentInfo {
    id: string;
    name: string;
    cwd: string;
    status: string;
  }

  let {
    agents,
    selectedId,
    onselect,
    onspawn,
    onkill,
    onrefresh,
    onsettings,
    onprojectsettings,
  }: {
    agents: AgentInfo[];
    selectedId: string | null;
    onselect: (id: string) => void;
    onspawn: (cwd: string) => void;
    onkill: (id: string) => void;
    onrefresh: (id: string) => void;
    onsettings: () => void;
    onprojectsettings: (cwd: string) => void;
  } = $props();

  let contextMenuAgent: string | null = $state(null);
  let contextMenuPos = $state({ x: 0, y: 0 });

  function handleContextMenu(e: MouseEvent, id: string) {
    e.preventDefault();
    contextMenuAgent = id;
    contextMenuPos = { x: e.clientX, y: e.clientY };
  }

  function handleContextKill() {
    if (contextMenuAgent) {
      onkill(contextMenuAgent);
      contextMenuAgent = null;
    }
  }

  function closeContextMenu() {
    contextMenuAgent = null;
  }

  async function handleAdd() {
    const selected = await open({
      directory: true,
      title: 'Select project directory',
    });
    if (selected) {
      onspawn(selected);
    }
  }
</script>

<svelte:window onclick={closeContextMenu} />

<div class="sidebar">
  <div class="sidebar-header">
    <span class="logo">nefor</span>
    <button tabindex="-1" class="settings-btn" onclick={onsettings} title="Settings">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="3"></circle>
        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
      </svg>
    </button>
  </div>

  <div class="agent-list">
    {#if agents.length === 0}
      <div class="empty-state">
        No agents running.<br />Click + to start.
      </div>
    {/if}

    {#each agents as agent (agent.id)}
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="agent-entry"
        class:selected={agent.id === selectedId}
        onclick={() => onselect(agent.id)}
        oncontextmenu={(e) => handleContextMenu(e, agent.id)}
      >
        <div class="agent-row">
          <span class="status-dot" class:running={agent.status === 'running'} class:exited={agent.status !== 'running'}></span>
          <span class="agent-name">{agent.name}</span>
          <button
            tabindex="-1"
            class="refresh-btn"
            onclick={(e: MouseEvent) => { e.stopPropagation(); onrefresh(agent.id); }}
            title="Refresh terminal"
          >
            <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M21 2v6h-6"></path>
              <path d="M3 12a9 9 0 0 1 15-6.7L21 8"></path>
              <path d="M3 22v-6h6"></path>
              <path d="M21 12a9 9 0 0 1-15 6.7L3 16"></path>
            </svg>
          </button>
          <button
            tabindex="-1"
            class="cog-btn"
            onclick={(e: MouseEvent) => { e.stopPropagation(); onprojectsettings(agent.cwd); }}
            title="Project settings"
          >
            <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="12" cy="12" r="3"></circle>
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
            </svg>
          </button>
          <button
            tabindex="-1"
            class="kill-btn"
            onclick={(e: MouseEvent) => { e.stopPropagation(); onkill(agent.id); }}
            title="Kill agent"
          >&times;</button>
        </div>
        <div class="agent-cwd">{agent.cwd}</div>
      </div>
    {/each}
  </div>

  {#if contextMenuAgent}
    <div class="context-menu" style="left: {contextMenuPos.x}px; top: {contextMenuPos.y}px;">
      <button tabindex="-1" class="context-item" onclick={handleContextKill}>Kill agent</button>
    </div>
  {/if}

  <button tabindex="-1" class="add-btn" onclick={handleAdd}>+</button>
</div>

<style>
  .sidebar {
    width: 240px;
    min-width: 240px;
    height: 100%;
    background: var(--bg-secondary);
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border-primary);
    user-select: none;
  }

  .sidebar-header {
    padding: 16px 16px 12px;
    border-bottom: 1px solid var(--border-secondary);
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .settings-btn {
    background: none;
    border: none;
    color: var(--text-dimmed);
    cursor: pointer;
    padding: 2px;
    line-height: 1;
    display: flex;
    align-items: center;
    transition: color 0.1s;
  }

  .settings-btn:hover {
    color: var(--text-primary);
  }

  .logo {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-dimmed);
    letter-spacing: 0.05em;
    text-transform: lowercase;
  }

  .agent-list {
    flex: 1;
    overflow-y: auto;
    padding: 4px 0;
  }

  .empty-state {
    padding: 24px 16px;
    color: var(--text-dimmed);
    font-size: 13px;
    line-height: 1.5;
    text-align: center;
  }

  .agent-entry {
    display: block;
    width: 100%;
    padding: 10px 16px;
    background: none;
    border: none;
    cursor: pointer;
    text-align: left;
    color: var(--text-primary);
    font-family: inherit;
    font-size: inherit;
    transition: background 0.1s;
  }

  .agent-entry:hover {
    background: var(--bg-hover);
  }

  .agent-entry.selected {
    background: var(--bg-tertiary);
  }

  .agent-row {
    display: flex;
    align-items: center;
    gap: 8px;
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

  .agent-name {
    font-weight: 600;
    font-size: 13px;
    color: var(--text-primary);
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .refresh-btn,
  .cog-btn {
    background: none;
    border: none;
    color: var(--text-dimmed);
    cursor: pointer;
    padding: 2px;
    line-height: 1;
    display: flex;
    align-items: center;
    opacity: 0;
    transition: opacity 0.1s, color 0.1s;
  }

  .agent-entry:hover .refresh-btn,
  .agent-entry:hover .cog-btn {
    opacity: 1;
  }

  .refresh-btn:hover,
  .cog-btn:hover {
    color: var(--text-primary);
  }

  .kill-btn {
    background: none;
    border: none;
    color: var(--text-dimmed);
    font-size: 16px;
    cursor: pointer;
    padding: 0 2px;
    line-height: 1;
    opacity: 0;
    transition: opacity 0.1s, color 0.1s;
  }

  .agent-entry:hover .kill-btn {
    opacity: 1;
  }

  .kill-btn:hover {
    color: var(--danger);
  }

  .agent-cwd {
    font-size: 11px;
    color: var(--text-dimmed);
    margin-top: 2px;
    margin-left: 14px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .context-menu {
    position: fixed;
    background: var(--bg-tertiary);
    border: 1px solid #333333;
    border-radius: 4px;
    padding: 4px 0;
    z-index: 1000;
    min-width: 120px;
  }

  .context-item {
    display: block;
    width: 100%;
    padding: 6px 12px;
    background: none;
    border: none;
    color: var(--text-primary);
    font-size: 12px;
    cursor: pointer;
    text-align: left;
    font-family: inherit;
  }

  .context-item:hover {
    background: var(--btn-hover);
  }

  .add-btn {
    width: 100%;
    padding: 12px;
    background: none;
    border: none;
    border-top: 1px solid var(--border-secondary);
    color: var(--text-secondary);
    font-size: 20px;
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
    font-family: inherit;
  }

  .add-btn:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
</style>
