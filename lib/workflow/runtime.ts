import type { DisguiseConfig, DisguiseExport, DispatchOpts, Effect, RuntimeHost, WorkflowContext } from "./types.ts";

const sleep = (ms: number): Promise<void> => new Promise(r => setTimeout(r, ms));

export async function dispatch(
  initial: Effect[],
  ctx: WorkflowContext,
  opts?: DispatchOpts,
): Promise<void> {
  const queue: Effect[] = [...initial];
  const running = new Set<Promise<void>>();
  const maxConcurrency = opts?.concurrency ?? 1;

  while (queue.length > 0 || running.size > 0) {
    if (running.size >= maxConcurrency) {
      await Promise.race(running);
    }

    if (opts?.maxQueueDepth && queue.length > opts.maxQueueDepth) {
      ctx.log(`queue overflow: ${queue.length} effects`);
      break;
    }

    // Pick the highest-priority effect (depth-first: later pipeline stages before new work)
    let bestIdx = 0;
    for (let i = 1; i < queue.length; i++) {
      if ((queue[i].priority ?? 0) > (queue[bestIdx].priority ?? 0)) bestIdx = i;
    }
    const effect = queue.splice(bestIdx, 1)[0];
    if (!effect) {
      if (running.size > 0) await Promise.race(running);
      continue;
    }

    const task = (async () => {
      if (opts?.minDelayMs) await sleep(opts.minDelayMs);
      try {
        const next = await effect.handle(ctx);
        queue.push(...next);
      } catch (err) {
        ctx.log(`effect ${effect.type} threw: ${err}`);
      }
    })();

    if (maxConcurrency <= 1) {
      await task;
    } else {
      const tracked = task.finally(() => running.delete(tracked));
      running.add(tracked);
    }
  }
}

export type Runtime = {
  activate(config: DisguiseConfig, host: RuntimeHost): WorkflowContext;
  dispatch(effects: Effect[]): void;
  getContext(): WorkflowContext | null;
};

export function createRuntime(): Runtime {
  let ctx: WorkflowContext | null = null;
  let queue: Effect[] = [];
  let loopRunning = false;

  const startLoop = async () => {
    if (!ctx || loopRunning) return;
    loopRunning = true;
    try {
      const toProcess = queue.splice(0);
      await dispatch(toProcess, ctx, ctx.config.dispatchOpts);
    } finally {
      loopRunning = false;
      if (queue.length > 0 && ctx) startLoop();
    }
  };

  return {
    activate(config, host) {
      const state = config.state ? config.state() : {};
      ctx = {
        config,
        workDir: host.cwd,
        piDir: host.piDir,
        state,
        sendMessage: (content, opts) => host.sendMessage(content, opts),
        log: (message) => host.sendMessage(`[workflow] ${message}`, { display: false }),
        notifyUser: (message, type) => host.notifyUser(message, type),
        spawn: (agent, prompt, opts) => host.spawnAgent(agent, prompt, opts),
        skill: (name, args) => host.runSkill(name, args),
        backgroundSkill: async (name, args) => {
          const handle = host.runSkillBackground(name, args);
          if (!handle) throw new Error(`skill "${name}" not found`);
          return handle;
        },
        openUrl: (url) => host.openUrl(url),
        registerTool: (spec) => host.registerTool(spec),
        removeTool: (name) => host.removeTool(name),
        setActiveTools: (tools) => host.setActiveTools(tools),
        input: (prompt, opts) => host.input(prompt, opts),
        select: (title, options) => host.select(title, options),
        setWidget: (key, lines) => host.setWidget(key, lines),
        dispatch: (effects) => {
          queue.push(...effects);
          if (!loopRunning) startLoop();
        },
      };
      return ctx;
    },
    dispatch(effects) {
      queue.push(...effects);
      if (!loopRunning && ctx) startLoop();
    },
    getContext() { return ctx; },
  };
}

export function defineDisguise(config: DisguiseExport): DisguiseExport {
  return config;
}
