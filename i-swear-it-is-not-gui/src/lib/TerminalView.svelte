<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from '@xterm/xterm';
  import { FitAddon } from '@xterm/addon-fit';
  import { WebglAddon } from '@xterm/addon-webgl';
  import { invoke, Channel } from '@tauri-apps/api/core';
  import '@xterm/xterm/css/xterm.css';

  let {
    agentId,
    cwd,
    visible,
    fontFamily = "'JetBrainsMono NFM', monospace",
    fontSize = 14,
    theme = { background: '#0a0a0a', foreground: '#e0e0e0' },
    refreshSignal = 0,
    onspawned,
    onexit,
  }: {
    agentId: string | null;
    cwd: string;
    visible: boolean;
    fontFamily?: string;
    fontSize?: number;
    theme?: { background: string; foreground: string };
    refreshSignal?: number;
    onspawned: (id: string) => void;
    onexit: (id: string) => void;
  } = $props();

  let containerEl: HTMLDivElement;
  let terminal: Terminal;
  let fitAddon: FitAddon;
  let myAgentId: string | null = null;

  $effect(() => {
    if (visible && fitAddon && terminal) {
      requestAnimationFrame(() => {
        fitAddon.fit();
        terminal.focus();
      });
    }
  });

  $effect(() => {
    if (terminal && fitAddon) {
      terminal.options.fontSize = fontSize;
      terminal.options.fontFamily = fontFamily;
      requestAnimationFrame(() => {
        fitAddon.fit();
      });
    }
  });

  $effect(() => {
    if (terminal && theme) {
      terminal.options.theme = theme;
      // Force full repaint — WebGL renderer caches colors
      terminal.refresh(0, terminal.rows - 1);
    }
  });

  // Fake resize to trigger SIGWINCH → forces TUI app to redraw
  $effect(() => {
    const sig = refreshSignal;
    if (sig > 0 && myAgentId && terminal) {
      const cols = terminal.cols;
      const rows = terminal.rows;
      invoke('resize_pty', { agentId: myAgentId, cols, rows: Math.max(rows - 1, 1) });
      setTimeout(() => {
        invoke('resize_pty', { agentId: myAgentId, cols, rows });
      }, 50);
    }
  });

  onMount(async () => {
    terminal = new Terminal({
      fontFamily: fontFamily,
      fontSize: fontSize,
      cursorBlink: true,
      theme: theme,
    });

    fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(containerEl);

    try {
      terminal.loadAddon(new WebglAddon());
    } catch {}

    fitAddon.fit();

    // Wire resize BEFORE spawning so we catch any early resize events
    terminal.onResize(({ cols, rows }) => {
      if (myAgentId) {
        invoke('resize_pty', { agentId: myAgentId, cols, rows });
      }
    });

    const channel = new Channel<{ type: string; payload: any }>();
    channel.onmessage = (event) => {
      if (event.type === 'Output') {
        terminal.write(event.payload);
      } else if (event.type === 'Exit') {
        terminal.write(`\r\n[Process exited with code ${event.payload}]\r\n`);
        if (myAgentId) {
          onexit(myAgentId);
        }
      }
    };

    // Pass actual terminal dimensions so the PTY starts at the right size
    myAgentId = await invoke<string>('spawn_agent', {
      cwd,
      cols: terminal.cols,
      rows: terminal.rows,
      onEvent: channel,
    });

    onspawned(myAgentId);

    terminal.onData((data) => {
      invoke('write_input', { agentId: myAgentId, data });
    });

    window.addEventListener('resize', handleResize);
    handleResize();
    terminal.focus();
  });

  function handleResize() {
    if (fitAddon && visible) {
      fitAddon.fit();
    }
  }

  onDestroy(() => {
    window.removeEventListener('resize', handleResize);
    if (myAgentId) {
      invoke('kill_agent', { agentId: myAgentId });
    }
    terminal?.dispose();
  });
</script>

<div
  bind:this={containerEl}
  class="terminal-container"
  class:hidden={!visible}
></div>

<style>
  .terminal-container {
    width: 100%;
    height: 100%;
  }

  .terminal-container.hidden {
    display: none;
  }
</style>
