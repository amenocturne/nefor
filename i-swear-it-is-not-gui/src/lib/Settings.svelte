<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import Combobox from './Combobox.svelte';

  interface ModelConfig {
    orchestrator: string;
    worker: string;
    reviewer: string;
    explorer: string;
    tester: string;
    promptEngineer: string;
  }

  interface ConfigData {
    provider: string;
    models: ModelConfig;
  }

  interface OpenedProject {
    path: string;
    lastOpened: string;
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
    theme: string;
    askToInstallPi: boolean;
    confirmKillAgent: boolean;
    openedProjects: OpenedProject[];
    keybindings: Keybindings;
  }

  interface ProjectConfig {
    provider: string;
    models: ModelConfig;
  }

  interface ProviderInfo {
    id: string;
    label: string;
  }

  interface ModelInfo {
    id: string;
    label: string;
  }

  let systemFonts: { id: string; label: string }[] = $state([]);

  type ModelRole = keyof ModelConfig;
  type KeybindingKey = keyof Keybindings;

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

  const SHORTCUT_LABELS: { key: KeybindingKey; label: string }[] = [
    { key: 'settings', label: 'Settings' },
    { key: 'newAgent', label: 'New Agent' },
    { key: 'spotlight', label: 'Agent Switcher' },
    { key: 'killAgent', label: 'Kill Agent' },
    { key: 'nextAgent', label: 'Next Agent' },
    { key: 'prevAgent', label: 'Previous Agent' },
    { key: 'zoomIn', label: 'Zoom In' },
    { key: 'zoomOut', label: 'Zoom Out' },
    { key: 'zoomReset', label: 'Reset Zoom' },
    { key: 'refreshTerminal', label: 'Refresh Terminal' },
  ];

  const isMac = navigator.platform.toUpperCase().indexOf('MAC') >= 0;

  let {
    configDir,
    piDir,
    onclose,
    initialTab = 'general',
    focusProject = null,
  }: {
    configDir: string;
    piDir: string | null;
    onclose: () => void;
    initialTab?: 'general' | 'projects' | 'shortcuts';
    focusProject?: string | null;
  } = $props();

  const roles: { key: ModelRole; label: string; desc: string }[] = [
    { key: 'orchestrator', label: 'Orchestrator', desc: 'Leads multi-agent tasks' },
    { key: 'worker', label: 'Worker', desc: 'Implements code changes' },
    { key: 'reviewer', label: 'Reviewer', desc: 'Reviews code quality' },
    { key: 'explorer', label: 'Explorer', desc: 'Explores codebases' },
    { key: 'tester', label: 'Tester', desc: 'Writes and runs tests' },
    { key: 'promptEngineer', label: 'Prompt Engineer', desc: 'Refines prompts' },
  ];

  // --- General tab state ---
  let activeTab: 'general' | 'projects' | 'shortcuts' = $state('general');
  let activeConfig: 'test' | 'prod' = $state('test');
  let provider = $state('');
  let models: ModelConfig = $state({
    orchestrator: '',
    worker: '',
    reviewer: '',
    explorer: '',
    tester: '',
    promptEngineer: '',
  });
  let loading = $state(true);
  let saving = $state(false);
  let error: string | null = $state(null);

  // --- Preferences state ---
  let prefs: Preferences = $state({
    font: 'JetBrainsMono NFM',
    theme: 'dark',
    askToInstallPi: true,
    confirmKillAgent: true,
    openedProjects: [],
    keybindings: { ...DEFAULT_KEYBINDINGS } as Keybindings,
  });

  // --- Model registry ---
  let allModels: ModelInfo[] = $state([]);

  // --- Projects tab state ---
  let expandedProject: string | null = $state(null);
  let projectConfigs: Record<string, ProjectConfig | null> = $state({});
  let projectHasPi: Record<string, boolean> = $state({});
  let projectSaving: Record<string, boolean> = $state({});

  // --- Shortcuts tab state ---
  let recordingAction: KeybindingKey | null = $state(null);

  function configPath(name: string): string {
    return `${configDir}/${name}.yaml`;
  }

  async function loadSystemFonts() {
    try {
      const fonts = await invoke<string[]>('list_system_fonts');
      systemFonts = fonts.map((f) => ({ id: f, label: f }));
    } catch {
      systemFonts = [];
    }
  }

  async function loadAllModels() {
    try {
      const providers = await invoke<ProviderInfo[]>('list_providers');
      const modelLists = await Promise.all(
        providers.map((p) => invoke<ModelInfo[]>('list_models', { provider: p.id }))
      );
      allModels = modelLists.flat();
    } catch {
      allModels = [];
    }
  }

  function normalizeProjectConfig(raw: any): ProjectConfig {
    const provider = raw.provider || raw.defaultProvider || '';
    const defaultModel = raw.defaultModel || '';
    const models = raw.models || {};
    return {
      provider,
      models: {
        orchestrator: models.orchestrator || defaultModel,
        worker: models.worker || defaultModel,
        reviewer: models.reviewer || defaultModel,
        explorer: models.explorer || defaultModel,
        tester: models.tester || defaultModel,
        promptEngineer: models.promptEngineer || defaultModel,
      },
    };
  }

  function setTheme(t: string) {
    prefs.theme = t;
    document.documentElement.setAttribute('data-theme', t);
    invoke('save_preferences', { prefs }).catch(() => {});
  }

  async function loadConfig() {
    loading = true;
    error = null;
    try {
      if (piDir) {
        try {
          const name = await invoke<string>('get_active_config_name', { piDir });
          if (name === 'test' || name === 'prod') {
            activeConfig = name;
          }
        } catch {
          // piDir may not have agentic-kit.json yet
        }
      }
      const config = await invoke<ConfigData>('get_config', {
        configPath: configPath(activeConfig),
      });
      provider = config.provider;
      models = { ...config.models };
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function loadPreferences() {
    try {
      const p = await invoke<Preferences>('get_preferences');
      if (!p.keybindings) {
        p.keybindings = { ...DEFAULT_KEYBINDINGS };
      }
      if (!p.theme) {
        p.theme = 'dark';
      }
      prefs = p;
    } catch {
      // use defaults
    }
  }

  async function switchConfig(name: 'test' | 'prod') {
    activeConfig = name;
    await loadConfig();
  }

  async function handleSave() {
    saving = true;
    error = null;
    try {
      // Save nefor config
      await invoke('save_config', {
        configPath: configPath(activeConfig),
        config: { provider, models },
      });
      if (piDir) {
        try {
          await invoke('set_active_config_name', { piDir, name: activeConfig });
        } catch {
          // Non-critical
        }
      }
      // Save preferences
      await invoke('save_preferences', { prefs });
      onclose();
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  function applyToAll() {
    const first = models.orchestrator;
    models = {
      orchestrator: first,
      worker: first,
      reviewer: first,
      explorer: first,
      tester: first,
      promptEngineer: first,
    };
  }

  // --- Projects tab ---

  async function toggleProjectExpand(path: string) {
    if (expandedProject === path) {
      expandedProject = null;
      return;
    }
    expandedProject = path;
    await loadProjectConfig(path);
  }

  async function loadProjectConfig(path: string) {
    try {
      const hasPi = await invoke<boolean>('check_pi_config', { cwd: path });
      projectHasPi = { ...projectHasPi, [path]: hasPi };
      if (hasPi) {
        const raw = await invoke<any>('get_project_config', { projectPath: path });
        projectConfigs = { ...projectConfigs, [path]: normalizeProjectConfig(raw) };
        const pConfig = projectConfigs[path];
      } else {
        projectConfigs = { ...projectConfigs, [path]: null };
      }
    } catch {
      projectConfigs = { ...projectConfigs, [path]: null };
      projectHasPi = { ...projectHasPi, [path]: false };
    }
  }

  async function installProjectPi(path: string) {
    try {
      await invoke('install_pi_config', { cwd: path });
      await loadProjectConfig(path);
    } catch (e) {
      console.error('Install failed:', e);
    }
  }

  async function saveProjectConfig(path: string) {
    const config = projectConfigs[path];
    if (!config) return;
    projectSaving = { ...projectSaving, [path]: true };
    try {
      await invoke('save_project_config', { projectPath: path, config });
    } catch (e) {
      console.error('Save failed:', e);
    } finally {
      projectSaving = { ...projectSaving, [path]: false };
    }
  }

  function removeProject(path: string) {
    prefs.openedProjects = prefs.openedProjects.filter((p) => p.path !== path);
    if (expandedProject === path) expandedProject = null;
    // Save immediately so removal persists
    invoke('save_preferences', { prefs }).catch(() => {});
  }

  function updateProjectConfigField(path: string, field: string, value: string) {
    const config = projectConfigs[path];
    if (!config) return;
    if (field === 'provider') {
      projectConfigs = { ...projectConfigs, [path]: { ...config, provider: value } };
    } else {
      const newModels = { ...config.models, [field]: value };
      projectConfigs = { ...projectConfigs, [path]: { ...config, models: newModels } };
    }
  }

  async function checkAllProjectsPi() {
    for (const project of prefs.openedProjects) {
      try {
        const hasPi = await invoke<boolean>('check_pi_config', { cwd: project.path });
        projectHasPi = { ...projectHasPi, [project.path]: hasPi };
      } catch {
        projectHasPi = { ...projectHasPi, [project.path]: false };
      }
    }
  }

  // --- Shortcuts tab ---

  function formatBinding(binding: string): string[] {
    const parts = binding.toLowerCase().split('+');
    return parts.map((p) => {
      if (p === 'meta') return isMac ? '\u2318' : 'Ctrl';
      if (p === 'shift') return isMac ? '\u21E7' : 'Shift';
      if (p === 'alt') return isMac ? '\u2325' : 'Alt';
      if (p === ',') return ',';
      if (p === '=') return '=';
      if (p === '-') return '-';
      if (p === '[') return '[';
      if (p === ']') return ']';
      return p.toUpperCase();
    });
  }

  function startRecording(action: KeybindingKey) {
    recordingAction = action;
  }

  function cancelRecording() {
    recordingAction = null;
  }

  function handleShortcutKeydown(e: KeyboardEvent) {
    if (!recordingAction) return;

    e.preventDefault();
    e.stopPropagation();

    if (e.key === 'Escape') {
      cancelRecording();
      return;
    }

    // Only capture when a modifier is held (ignore bare modifier presses)
    const hasModifier = e.metaKey || e.ctrlKey || e.altKey || e.shiftKey;
    const isModifierOnly = ['Meta', 'Control', 'Alt', 'Shift'].includes(e.key);
    if (!hasModifier || isModifierOnly) return;

    const parts: string[] = [];
    if (e.metaKey || e.ctrlKey) parts.push('meta');
    if (e.shiftKey) parts.push('shift');
    if (e.altKey) parts.push('alt');
    parts.push(e.key.toLowerCase());

    const binding = parts.join('+');
    prefs.keybindings = { ...prefs.keybindings, [recordingAction]: binding };
    recordingAction = null;

    // Auto-save keybindings
    invoke('save_preferences', { prefs }).catch(() => {});
  }

  function resetKeybindings() {
    prefs.keybindings = { ...DEFAULT_KEYBINDINGS };
    invoke('save_preferences', { prefs }).catch(() => {});
  }

  onMount(async () => {
    activeTab = initialTab;
    await Promise.all([loadConfig(), loadPreferences(), loadAllModels(), loadSystemFonts()]);
    // Check pi status for all projects after prefs are loaded
    await checkAllProjectsPi();
    if (focusProject) {
      activeTab = 'projects';
      setTimeout(() => toggleProjectExpand(focusProject!), 100);
    }
  });
</script>

<svelte:window onkeydown={handleShortcutKeydown} />
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onclick={onclose}>
  <div class="panel" onclick={(e) => e.stopPropagation()}>
    <div class="panel-header">
      <span class="panel-title">Settings</span>
      <button tabindex="-1" class="close-btn" onclick={onclose}>&times;</button>
    </div>

    <div class="tab-bar">
      <button
        class="tab-btn"
        class:active={activeTab === 'general'}
        onclick={() => activeTab = 'general'}
      >General</button>
      <button
        class="tab-btn"
        class:active={activeTab === 'projects'}
        onclick={() => activeTab = 'projects'}
      >Projects</button>
      <button
        class="tab-btn"
        class:active={activeTab === 'shortcuts'}
        onclick={() => activeTab = 'shortcuts'}
      >Shortcuts</button>
    </div>

    {#if loading}
      <div class="loading">Loading...</div>
    {:else}
      <div class="panel-body">
        {#if error}
          <div class="error">{error}</div>
        {/if}

        {#if activeTab === 'general'}
          <!-- GENERAL TAB -->
          <section>
            <h3>Font</h3>
            <Combobox
              value={prefs.font}
              options={systemFonts}
              placeholder="JetBrainsMono NFM"
              onchange={(v) => prefs.font = v}
            />
            <div class="field-desc">Nerd Font recommended for full glyph support.</div>
          </section>

          <section>
            <!-- svelte-ignore a11y_label_has_associated_control -->
            <label class="checkbox-row">
              <input type="checkbox" bind:checked={prefs.askToInstallPi} />
              <span>Ask to install Pi config when opening unconfigured directories</span>
            </label>
          </section>

          <section>
            <!-- svelte-ignore a11y_label_has_associated_control -->
            <label class="checkbox-row">
              <input type="checkbox" bind:checked={prefs.confirmKillAgent} />
              <span>Confirm before killing an agent</span>
            </label>
          </section>

          <section>
            <h3>Active Configuration</h3>
            <div class="config-toggle">
              <button
                class="toggle-btn"
                class:active={activeConfig === 'test'}
                onclick={() => switchConfig('test')}
              >test</button>
              <button
                class="toggle-btn"
                class:active={activeConfig === 'prod'}
                onclick={() => switchConfig('prod')}
              >prod</button>
            </div>
            <div class="config-hint">
              {activeConfig === 'test' ? 'OpenRouter / Claude' : 'Nestor / Qwen'}
            </div>
          </section>

          <section>
            <h3>Theme</h3>
            <div class="config-toggle">
              <button tabindex="-1" class="toggle-btn" class:active={prefs.theme === 'dark'} onclick={() => setTheme('dark')}>Dark</button>
              <button tabindex="-1" class="toggle-btn" class:active={prefs.theme === 'light'} onclick={() => setTheme('light')}>Light</button>
            </div>
          </section>

          <section>
            <div class="section-header">
              <h3>Role-based Models</h3>
              <button tabindex="-1" class="small-btn" onclick={applyToAll}>Use same for all</button>
            </div>

            {#each roles as role (role.key)}
              <div class="role-field">
                <div class="role-info">
                  <span class="role-label">{role.label}</span>
                  <span class="role-desc">{role.desc}</span>
                </div>
                <Combobox
                  value={models[role.key]}
                  options={allModels}
                  onchange={(v) => models[role.key] = v}
                />
              </div>
            {/each}
          </section>

          <div class="actions">
            <button tabindex="-1" class="action-btn save" onclick={handleSave} disabled={saving}>
              {saving ? 'Saving...' : 'Save'}
            </button>
            <button tabindex="-1" class="action-btn cancel" onclick={onclose}>Cancel</button>
          </div>

        {:else if activeTab === 'projects'}
          <!-- PROJECTS TAB -->
          <section>
            {#if prefs.openedProjects.length === 0}
              <div class="empty-projects">No previously opened projects.</div>
            {:else}
              {#each prefs.openedProjects as project (project.path)}
                <div class="project-entry">
                  <div class="project-row">
                    <span
                      class="pi-dot"
                      class:has-pi={projectHasPi[project.path] === true}
                      class:no-pi={projectHasPi[project.path] === false}
                      title={projectHasPi[project.path] ? '.pi/ configured' : 'No .pi/ config'}
                    ></span>
                    <div class="project-info">
                      <span class="project-path">{project.path}</span>
                      <span class="project-date">Last opened: {project.lastOpened}</span>
                    </div>
                    <button
                      tabindex="-1"
                      class="icon-btn cogwheel"
                      onclick={() => toggleProjectExpand(project.path)}
                      title="Project settings"
                    >
                      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <circle cx="12" cy="12" r="3"></circle>
                        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
                      </svg>
                    </button>
                    <button
                      tabindex="-1"
                      class="icon-btn remove"
                      onclick={() => removeProject(project.path)}
                      title="Remove from list"
                    >&times;</button>
                  </div>

                  {#if expandedProject === project.path}
                    <div class="project-config">
                      {#if projectHasPi[project.path] === false}
                        <div class="not-configured">
                          <span>Not configured</span>
                          <button tabindex="-1" class="small-btn" onclick={() => installProjectPi(project.path)}>Install .pi/ config</button>
                        </div>
                      {:else if projectConfigs[project.path]}
                        <div class="config-fields">
                          {#each roles as role (role.key)}
                            <div class="config-field">
                              <span class="config-label">{role.label}</span>
                              <Combobox
                                value={projectConfigs[project.path]!.models[role.key]}
                                options={allModels}
                                onchange={(v) => updateProjectConfigField(project.path, role.key, v)}
                              />
                            </div>
                          {/each}
                          <div class="config-actions">
                            <button
                              class="action-btn save"
                              onclick={() => saveProjectConfig(project.path)}
                              disabled={projectSaving[project.path]}
                            >
                              {projectSaving[project.path] ? 'Saving...' : 'Save Project Config'}
                            </button>
                          </div>
                        </div>
                      {:else}
                        <div class="loading-inline">Loading config...</div>
                      {/if}
                    </div>
                  {/if}
                </div>
              {/each}
            {/if}
          </section>

        {:else if activeTab === 'shortcuts'}
          <!-- SHORTCUTS TAB -->
          <section>
            <div class="shortcuts-list">
              {#each SHORTCUT_LABELS as shortcut (shortcut.key)}
                <div class="shortcut-row">
                  <span class="shortcut-action">{shortcut.label}</span>
                  <div class="shortcut-right">
                    {#if recordingAction === shortcut.key}
                      <div class="shortcut-recording">
                        <span class="recording-text">Press a key combo...</span>
                      </div>
                      <button tabindex="-1" class="small-btn" onclick={cancelRecording}>Cancel</button>
                    {:else}
                      <div class="key-combo">
                        {#each formatBinding(prefs.keybindings[shortcut.key]) as part, i}
                          {#if i > 0}<span class="key-separator">+</span>{/if}
                          <span class="key-badge">{part}</span>
                        {/each}
                      </div>
                      <button tabindex="-1" class="small-btn" onclick={() => startRecording(shortcut.key)}>Rebind</button>
                    {/if}
                  </div>
                </div>
              {/each}

              <div class="shortcut-row shortcut-static">
                <span class="shortcut-action">Agent 1-9</span>
                <div class="shortcut-right">
                  <div class="key-combo">
                    <span class="key-badge">{isMac ? '\u2318' : 'Ctrl'}</span>
                    <span class="key-separator">+</span>
                    <span class="key-badge">1-9</span>
                  </div>
                </div>
              </div>

              <div class="shortcut-row shortcut-static">
                <span class="shortcut-action">Close / Cancel</span>
                <div class="shortcut-right">
                  <div class="key-combo">
                    <span class="key-badge">Esc</span>
                  </div>
                </div>
              </div>
            </div>
          </section>

          <div class="actions">
            <button tabindex="-1" class="action-btn cancel" onclick={resetKeybindings}>Reset to defaults</button>
          </div>
        {/if}
      </div>
    {/if}
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
    z-index: 100;
  }

  .panel {
    background: var(--bg-secondary);
    border: 1px solid var(--border-primary);
    border-radius: 8px;
    width: 560px;
    max-height: 85vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px 12px;
  }

  .panel-title {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .close-btn {
    background: none;
    border: none;
    color: var(--text-dimmed);
    font-size: 20px;
    cursor: pointer;
    padding: 0 4px;
    line-height: 1;
  }

  .close-btn:hover {
    color: var(--text-primary);
  }

  .tab-bar {
    display: flex;
    gap: 4px;
    padding: 0 20px 12px;
    border-bottom: 1px solid var(--border-secondary);
  }

  .tab-btn {
    flex: 1;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border-primary);
    border-radius: 4px;
    color: var(--text-secondary);
    font-size: 13px;
    font-family: inherit;
    cursor: pointer;
    transition: background 0.1s, color 0.1s, border-color 0.1s;
  }

  .tab-btn:hover {
    background: var(--btn-hover);
    color: var(--text-primary);
  }

  .tab-btn.active {
    background: var(--accent-bg);
    border-color: var(--accent-border);
    color: var(--accent);
  }

  .loading {
    padding: 40px 20px;
    text-align: center;
    color: var(--text-dimmed);
    font-size: 13px;
  }

  .panel-body {
    padding: 20px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .error {
    padding: 8px 12px;
    background: var(--danger-bg);
    border: 1px solid var(--danger-border);
    border-radius: 4px;
    color: var(--danger);
    font-size: 12px;
  }

  section {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  h3 {
    margin: 0;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .field-desc {
    font-size: 11px;
    color: var(--text-dimmed);
    line-height: 1.4;
  }

  .checkbox-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--text-primary);
    cursor: pointer;
  }

  .checkbox-row input[type="checkbox"] {
    accent-color: var(--accent);
  }

  .config-toggle {
    display: flex;
    gap: 4px;
  }

  .toggle-btn {
    flex: 1;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border-primary);
    border-radius: 4px;
    color: var(--text-secondary);
    font-size: 13px;
    font-family: inherit;
    cursor: pointer;
    transition: background 0.1s, color 0.1s, border-color 0.1s;
  }

  .toggle-btn:hover {
    background: var(--btn-hover);
    color: var(--text-primary);
  }

  .toggle-btn.active {
    background: var(--accent-bg);
    border-color: var(--accent-border);
    color: var(--accent);
  }

  .config-hint {
    font-size: 11px;
    color: var(--text-dimmed);
  }

  .section-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .small-btn {
    padding: 4px 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border-primary);
    border-radius: 4px;
    color: var(--text-secondary);
    font-size: 11px;
    font-family: inherit;
    cursor: pointer;
    transition: background 0.1s, color 0.1s;
  }

  .small-btn:hover {
    background: var(--btn-hover);
    color: var(--text-primary);
  }

  .role-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 0;
  }

  .role-field:not(:last-child) {
    border-bottom: 1px solid var(--border-secondary);
  }

  .role-info {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .role-label {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .role-desc {
    font-size: 11px;
    color: var(--text-dimmed);
  }

  .actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    padding-top: 8px;
    border-top: 1px solid var(--border-secondary);
  }

  .action-btn {
    padding: 8px 20px;
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

  .save {
    background: var(--accent-bg);
    border-color: var(--accent-border);
    color: var(--accent);
  }

  .save:hover {
    background: var(--accent-hover);
  }

  .save:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* --- Projects tab --- */

  .empty-projects {
    padding: 24px 0;
    text-align: center;
    color: var(--text-dimmed);
    font-size: 13px;
  }

  .project-entry {
    border-bottom: 1px solid var(--border-secondary);
  }

  .project-entry:last-child {
    border-bottom: none;
  }

  .project-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 0;
  }

  .pi-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
    background: #333333;
  }

  .pi-dot.has-pi {
    background: var(--accent);
  }

  .pi-dot.no-pi {
    background: #555555;
  }

  .project-info {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .project-path {
    font-size: 13px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .project-date {
    font-size: 11px;
    color: var(--text-dimmed);
  }

  .icon-btn {
    background: none;
    border: none;
    color: var(--text-dimmed);
    cursor: pointer;
    padding: 4px;
    line-height: 1;
    display: flex;
    align-items: center;
    transition: color 0.1s;
  }

  .icon-btn:hover {
    color: var(--text-primary);
  }

  .icon-btn.remove {
    font-size: 16px;
  }

  .icon-btn.remove:hover {
    color: var(--danger);
  }

  .project-config {
    padding: 0 0 12px 16px;
  }

  .not-configured {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 0;
    font-size: 13px;
    color: var(--text-dimmed);
  }

  .config-fields {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .config-field {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .config-label {
    font-size: 12px;
    color: var(--text-secondary);
    min-width: 100px;
    flex-shrink: 0;
  }

  .config-actions {
    display: flex;
    justify-content: flex-end;
    padding-top: 8px;
  }

  .loading-inline {
    padding: 8px 0;
    color: var(--text-dimmed);
    font-size: 12px;
  }

  /* --- Shortcuts tab --- */

  .shortcuts-list {
    display: flex;
    flex-direction: column;
  }

  .shortcut-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 0;
    border-bottom: 1px solid var(--border-secondary);
  }

  .shortcut-row:last-child {
    border-bottom: none;
  }

  .shortcut-action {
    font-size: 13px;
    color: var(--text-primary);
    flex-shrink: 0;
  }

  .shortcut-static .shortcut-action {
    color: var(--text-secondary);
  }

  .shortcut-right {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .key-combo {
    display: flex;
    align-items: center;
    gap: 3px;
  }

  .key-badge {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 24px;
    height: 24px;
    padding: 0 6px;
    background: var(--bg-secondary);
    border: 1px solid var(--border-secondary);
    border-radius: 4px;
    color: #cccccc;
    font-size: 12px;
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    line-height: 1;
  }

  .key-separator {
    color: #555555;
    font-size: 11px;
  }

  .shortcut-recording {
    display: inline-flex;
    align-items: center;
    height: 24px;
    padding: 0 10px;
    border: 1px dashed var(--accent);
    border-radius: 4px;
    animation: pulse-border 1.5s ease-in-out infinite;
  }

  .recording-text {
    font-size: 12px;
    color: var(--accent);
  }

  @keyframes pulse-border {
    0%, 100% { border-color: var(--accent); }
    50% { border-color: var(--accent-border); }
  }
</style>
