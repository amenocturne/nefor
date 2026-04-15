<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { onMount } from 'svelte';
  import Sidebar from './lib/Sidebar.svelte';
  import TerminalView from './lib/TerminalView.svelte';
  import Settings from './lib/Settings.svelte';
  import InstallPrompt from './lib/InstallPrompt.svelte';
  import Spotlight from './lib/Spotlight.svelte';
  import NameInput from './lib/NameInput.svelte';
  import QuitConfirm from './lib/QuitConfirm.svelte';
  import KillConfirm from './lib/KillConfirm.svelte';

  // Resolved at runtime via Tauri resource path; falls back to relative for dev
  const NEFOR_CONFIG_DIR = '../config';
  const NEFOR_PI_DIR: string | null = null;
  const DEFAULT_FONT_SIZE = 14;

  interface AgentEntry {
    id: string | null;
    tempId: string;
    cwd: string;
    name: string;
    status: string;
    refreshSignal: number;
  }

  interface Keybindings {
    settings: string;
    newAgent: string;
    spotlight: string;
    killAgent: string;
    nextAgent: string;
    prevAgent: string;
    zoomIn: string;
    zoomOut: string;
    zoomReset: string;
    refreshTerminal: string;
  }

  interface Preferences {
    font: string;
    fontSize: number;
    theme: string;
    askToInstallPi: boolean;
    confirmKillAgent: boolean;
    openedProjects: { path: string; lastOpened: string }[];
    keybindings: Keybindings;
  }

  const DEFAULT_KEYBINDINGS: Keybindings = {
    settings: 'meta+,',
    newAgent: 'meta+k',
    spotlight: 'meta+o',
    killAgent: 'meta+w',
    nextAgent: 'meta+]',
    prevAgent: 'meta+[',
    zoomIn: 'meta+=',
    zoomOut: 'meta+-',
    zoomReset: 'meta+0',
    refreshTerminal: 'meta+shift+r',
  };

  const isMac = navigator.platform.toUpperCase().indexOf('MAC') >= 0;

  let agents: AgentEntry[] = $state([]);
  let selectedTempId: string | null = $state(null);
  let showSettings = $state(false);
  let settingsTab: 'general' | 'projects' | 'shortcuts' = $state('general');
  let keybindings: Keybindings = $state({ ...DEFAULT_KEYBINDINGS });
  let settingsFocusProject: string | null = $state(null);
  let fontFamily = $state("'JetBrainsMono NFM', monospace");
  let fontSize = $state(DEFAULT_FONT_SIZE);

  // Install prompt state
  let installPromptCwd: string | null = $state(null);

  // Spotlight state
  let showSpotlight = $state(false);

  // Name input state
  let namingCwd: string | null = $state(null);
  let namingDefaultName: string | null = $state(null);

  // Quit confirmation state
  let showQuitConfirm = $state(false);

  // Kill confirmation state
  let killConfirmAgentId: string | null = $state(null);
  let killConfirmAgentName: string | null = $state(null);
  let confirmKillAgent = $state(true);

  // Theme state
  let currentTheme = $state('dark');

  let tempCounter = 0;

  // Whether any modal is open -- suppresses most shortcuts
  let modalOpen = $derived(showSettings || installPromptCwd !== null || showSpotlight || namingCwd !== null || showQuitConfirm || killConfirmAgentId !== null);

  // Load preferences on startup
  loadPreferences();

  function applyTheme(theme: string) {
    currentTheme = theme;
    document.documentElement.setAttribute('data-theme', theme);
  }

  let terminalTheme = $derived(currentTheme === 'dark'
    ? { background: '#0a0a0a', foreground: '#e0e0e0' }
    : { background: '#ffffff', foreground: '#1a1a1a' }
  );

  onMount(() => {
    const appWindow = getCurrentWindow();
    appWindow.onCloseRequested(async (event) => {
      if (agents.length > 0) {
        event.preventDefault();
        showQuitConfirm = true;
      }
    });
  });

  async function handleQuitConfirm() {
    showQuitConfirm = false;
    const appWindow = getCurrentWindow();
    try {
      await appWindow.destroy();
    } catch {
      await appWindow.close();
    }
  }

  function handleQuitCancel() {
    showQuitConfirm = false;
  }

  function matchesBinding(e: KeyboardEvent, binding: string): boolean {
    const parts = binding.toLowerCase().split('+');
    const key = parts.pop()!;
    const needsMeta = parts.includes('meta');
    const needsShift = parts.includes('shift');
    const needsAlt = parts.includes('alt');

    const mod = isMac ? e.metaKey : e.ctrlKey;
    if (needsMeta && !mod) return false;
    if (!needsMeta && mod) return false;
    if (needsShift && !e.shiftKey) return false;
    if (!needsShift && e.shiftKey) return false;
    if (needsAlt && !e.altKey) return false;
    if (!needsAlt && e.altKey) return false;

    return e.key.toLowerCase() === key;
  }

  async function loadPreferences() {
    try {
      const prefs = await invoke<Preferences>('get_preferences');
      if (prefs.font && prefs.font.trim()) {
        fontFamily = `'${prefs.font}', 'JetBrainsMono NFM', monospace`;
      } else {
        fontFamily = "'JetBrainsMono NFM', monospace";
      }
      if (prefs.fontSize && prefs.fontSize > 0) {
        fontSize = prefs.fontSize;
      }
      if (prefs.keybindings) {
        keybindings = prefs.keybindings;
      }
      confirmKillAgent = prefs.confirmKillAgent !== false;
      applyTheme(prefs.theme || 'dark');
    } catch {
      fontFamily = "'JetBrainsMono NFM', monospace";
      fontSize = DEFAULT_FONT_SIZE;
      applyTheme('dark');
    }
  }

  async function saveFontSize(size: number) {
    try {
      const prefs = await invoke<Preferences>('get_preferences');
      prefs.fontSize = size;
      await invoke('save_preferences', { prefs });
    } catch {}
  }

  async function trackProject(cwd: string) {
    try {
      const prefs = await invoke<Preferences>('get_preferences');
      const today = new Date().toISOString().split('T')[0];
      const existing = prefs.openedProjects.find((p) => p.path === cwd);
      if (existing) {
        existing.lastOpened = today;
      } else {
        prefs.openedProjects.push({ path: cwd, lastOpened: today });
      }
      await invoke('save_preferences', { prefs });
    } catch {}
  }

  function generateDefaultName(cwd: string): string {
    const basename = cwd.split('/').pop() || cwd;
    const existingCount = agents.filter(
      (a) => a.name === basename || a.name.startsWith(basename + '-')
    ).length;
    return existingCount === 0 ? basename : `${basename}-${existingCount + 1}`;
  }

  function spawnAgent(cwd: string, customName?: string) {
    const name = customName || generateDefaultName(cwd);

    const tempId = `temp-${++tempCounter}`;
    const entry: AgentEntry = {
      id: null,
      tempId,
      cwd,
      name,
      status: 'running',
      refreshSignal: 0,
    };
    agents = [...agents, entry];
    selectedTempId = tempId;

    trackProject(cwd);
  }

  async function handleDirectorySelected(cwd: string) {
    try {
      const prefs = await invoke<Preferences>('get_preferences');
      if (!prefs.askToInstallPi) {
        showNameInput(cwd);
        return;
      }
      const hasPi = await invoke<boolean>('check_pi_config', { cwd });
      if (hasPi) {
        showNameInput(cwd);
      } else {
        installPromptCwd = cwd;
      }
    } catch {
      showNameInput(cwd);
    }
  }

  function showNameInput(cwd: string) {
    namingCwd = cwd;
    namingDefaultName = generateDefaultName(cwd);
  }

  function handleNameAccept(name: string) {
    const cwd = namingCwd!;
    namingCwd = null;
    namingDefaultName = null;
    spawnAgent(cwd, name);
  }

  function handleNameCancel() {
    namingCwd = null;
    namingDefaultName = null;
  }

  function handleInstallComplete() {
    const cwd = installPromptCwd!;
    installPromptCwd = null;
    showNameInput(cwd);
  }

  function handleInstallSkip() {
    const cwd = installPromptCwd!;
    installPromptCwd = null;
    showNameInput(cwd);
  }

  function handleInstallCancel() {
    installPromptCwd = null;
  }

  function handleSpawned(tempId: string, agentId: string) {
    agents = agents.map((a) =>
      a.tempId === tempId ? { ...a, id: agentId } : a
    );
  }

  function handleExit(agentId: string) {
    agents = agents.map((a) =>
      a.id === agentId ? { ...a, status: 'exited' } : a
    );
  }

  function selectAgent(id: string) {
    const agent = agents.find((a) => a.id === id);
    if (agent) {
      selectedTempId = agent.tempId;
    }
  }

  function requestKillAgent(id: string) {
    const agent = agents.find((a) => a.id === id);
    if (!agent) return;

    if (confirmKillAgent) {
      killConfirmAgentId = id;
      killConfirmAgentName = agent.name;
    } else {
      doKillAgent(id);
    }
  }

  async function doKillAgent(id: string) {
    const agent = agents.find((a) => a.id === id);
    if (!agent) return;

    try {
      await invoke('kill_agent', { agentId: id });
    } catch {}

    agents = agents.filter((a) => a.id !== id);

    if (selectedTempId === agent.tempId) {
      selectedTempId = agents.length > 0 ? agents[agents.length - 1].tempId : null;
    }
  }

  function handleKillConfirm() {
    const id = killConfirmAgentId;
    killConfirmAgentId = null;
    killConfirmAgentName = null;
    if (id) doKillAgent(id);
    // Reload prefs in case "Don't ask again" was checked
    loadPreferences();
  }

  function handleKillCancel() {
    killConfirmAgentId = null;
    killConfirmAgentName = null;
  }

  function killCurrentAgent() {
    const current = agents.find((a) => a.tempId === selectedTempId);
    if (current?.id) {
      requestKillAgent(current.id);
    }
  }

  function refreshAgent(id: string) {
    agents = agents.map((a) =>
      a.id === id ? { ...a, refreshSignal: a.refreshSignal + 1 } : a
    );
  }

  function refreshCurrentAgent() {
    const current = agents.find((a) => a.tempId === selectedTempId);
    if (current?.id) {
      refreshAgent(current.id);
    }
  }

  function openSettings(tab: 'general' | 'projects' | 'shortcuts' = 'general', focusProject: string | null = null) {
    settingsTab = tab;
    settingsFocusProject = focusProject;
    showSettings = true;
  }

  function closeSettings() {
    showSettings = false;
    settingsFocusProject = null;
    loadPreferences();
  }

  function switchToNextAgent() {
    if (agents.length <= 1) return;
    const idx = agents.findIndex((a) => a.tempId === selectedTempId);
    const next = (idx + 1) % agents.length;
    selectedTempId = agents[next].tempId;
  }

  function switchToPrevAgent() {
    if (agents.length <= 1) return;
    const idx = agents.findIndex((a) => a.tempId === selectedTempId);
    const prev = (idx - 1 + agents.length) % agents.length;
    selectedTempId = agents[prev].tempId;
  }

  function switchToAgentByIndex(index: number) {
    if (index >= 0 && index < agents.length) {
      selectedTempId = agents[index].tempId;
    }
  }

  function zoomIn() {
    fontSize = Math.min(fontSize + 1, 40);
    saveFontSize(fontSize);
  }

  function zoomOut() {
    fontSize = Math.max(fontSize - 1, 8);
    saveFontSize(fontSize);
  }

  function zoomReset() {
    fontSize = DEFAULT_FONT_SIZE;
    saveFontSize(fontSize);
  }

  async function triggerNewAgent() {
    const selected = await open({
      directory: true,
      title: 'Select project directory',
    });
    if (selected) {
      handleDirectorySelected(selected);
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    const mod = isMac ? e.metaKey : e.ctrlKey;

    // Suppress Tab entirely — prevents accidental button activation
    if (e.key === 'Tab') {
      e.preventDefault();
      return;
    }

    // Esc always works -- close modals in priority order
    if (e.key === 'Escape') {
      if (showQuitConfirm) { handleQuitCancel(); e.preventDefault(); return; }
      if (killConfirmAgentId) { handleKillCancel(); e.preventDefault(); return; }
      if (showSpotlight) { showSpotlight = false; e.preventDefault(); return; }
      if (namingCwd) { handleNameCancel(); e.preventDefault(); return; }
      if (installPromptCwd) { handleInstallCancel(); e.preventDefault(); return; }
      if (showSettings) { closeSettings(); e.preventDefault(); return; }
      return;
    }

    // When a modal is open, suppress all other shortcuts
    if (modalOpen) return;

    if (matchesBinding(e, keybindings.settings)) {
      e.preventDefault();
      openSettings('general');
      return;
    }

    if (matchesBinding(e, keybindings.newAgent)) {
      e.preventDefault();
      triggerNewAgent();
      return;
    }

    if (matchesBinding(e, keybindings.spotlight)) {
      e.preventDefault();
      if (agents.length > 0) {
        showSpotlight = true;
      }
      return;
    }

    if (matchesBinding(e, keybindings.killAgent)) {
      e.preventDefault();
      killCurrentAgent();
      return;
    }

    if (matchesBinding(e, keybindings.nextAgent)) {
      e.preventDefault();
      switchToNextAgent();
      return;
    }

    if (matchesBinding(e, keybindings.prevAgent)) {
      e.preventDefault();
      switchToPrevAgent();
      return;
    }

    if (matchesBinding(e, keybindings.zoomIn)) {
      e.preventDefault();
      zoomIn();
      return;
    }

    if (matchesBinding(e, keybindings.zoomOut)) {
      e.preventDefault();
      zoomOut();
      return;
    }

    if (matchesBinding(e, keybindings.zoomReset)) {
      e.preventDefault();
      zoomReset();
      return;
    }

    if (matchesBinding(e, keybindings.refreshTerminal)) {
      e.preventDefault();
      refreshCurrentAgent();
      return;
    }

    // Cmd/Ctrl + 1-9 → switch by index (not rebindable)
    if (mod && e.key >= '1' && e.key <= '9') {
      e.preventDefault();
      switchToAgentByIndex(parseInt(e.key) - 1);
      return;
    }
  }

  // Derive sidebar-friendly list
  let sidebarAgents = $derived(
    agents.map((a) => ({
      id: a.id || a.tempId,
      name: a.name,
      cwd: a.cwd,
      status: a.status,
    }))
  );

  let selectedSidebarId = $derived(
    (() => {
      const sel = agents.find((a) => a.tempId === selectedTempId);
      return sel ? (sel.id || sel.tempId) : null;
    })()
  );
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="layout">
  <Sidebar
    agents={sidebarAgents}
    selectedId={selectedSidebarId}
    onselect={selectAgent}
    onspawn={handleDirectorySelected}
    onkill={requestKillAgent}
    onrefresh={refreshAgent}
    onsettings={() => openSettings('general')}
    onprojectsettings={(cwd) => openSettings('projects', cwd)}
  />

  <div class="terminal-area">
    {#each agents as agent (agent.tempId)}
      <TerminalView
        agentId={agent.id}
        cwd={agent.cwd}
        visible={agent.tempId === selectedTempId}
        {fontFamily}
        {fontSize}
        theme={terminalTheme}
        refreshSignal={agent.refreshSignal}
        onspawned={(id) => handleSpawned(agent.tempId, id)}
        onexit={handleExit}
      />
    {/each}

    {#if agents.length === 0}
      <div class="no-terminal">
        <span>Select a directory to start</span>
      </div>
    {/if}
  </div>
</div>

{#if showSettings}
  <Settings
    configDir={NEFOR_CONFIG_DIR}
    piDir={NEFOR_PI_DIR}
    onclose={closeSettings}
    initialTab={settingsTab}
    focusProject={settingsFocusProject}
  />
{/if}

{#if installPromptCwd}
  <InstallPrompt
    cwd={installPromptCwd}
    oninstall={handleInstallComplete}
    onskip={handleInstallSkip}
    oncancel={handleInstallCancel}
  />
{/if}

{#if showSpotlight}
  <Spotlight
    agents={sidebarAgents}
    onselect={(id) => { selectAgent(id); showSpotlight = false; }}
    onclose={() => showSpotlight = false}
  />
{/if}

{#if namingCwd && namingDefaultName}
  <NameInput
    defaultName={namingDefaultName}
    onaccept={handleNameAccept}
    oncancel={handleNameCancel}
  />
{/if}

{#if showQuitConfirm}
  <QuitConfirm
    onconfirm={handleQuitConfirm}
    oncancel={handleQuitCancel}
  />
{/if}

{#if killConfirmAgentId && killConfirmAgentName}
  <KillConfirm
    agentName={killConfirmAgentName}
    onconfirm={handleKillConfirm}
    oncancel={handleKillCancel}
  />
{/if}

<style>
  .layout {
    display: flex;
    width: 100%;
    height: 100%;
  }

  .terminal-area {
    flex: 1;
    position: relative;
    overflow: hidden;
    background: var(--bg-primary);
  }

  .no-terminal {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--text-muted);
    font-size: 14px;
  }
</style>
