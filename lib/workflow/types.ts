export type Effect = {
  readonly type: string;
  /** Higher priority effects are picked before lower ones (default 0). */
  readonly priority?: number;
  handle(ctx: WorkflowContext): Promise<Effect[]>;
};

export interface WorkflowContext {
  readonly config: DisguiseConfig;
  readonly workDir: string;
  readonly piDir: string;
  state: Record<string, unknown>;
  sendMessage(content: string, opts?: MessageOpts): void;
  log(message: string): void;
  /** Toast notification visible to the user only — NOT added to the agent's context. */
  notifyUser(message: string, type?: "info" | "warning" | "error"): void;
  spawn(agent: AgentConfig, prompt: string, opts?: SpawnOpts): Promise<AgentResult>;
  skill(name: string, args: string[]): Promise<SkillResult>;
  backgroundSkill(name: string, args: string[]): Promise<BackgroundSkillHandle>;
  openUrl(url: string): void;
  registerTool(spec: ToolRegistration): void;
  removeTool(name: string): void;
  setActiveTools(tools: string[]): void;
  input(prompt: string, opts?: InputOpts): Promise<string>;
  dispatch(effects: Effect[]): void;
  setWidget(key: string, lines: string[] | undefined): void;
}

export type AgentConfig = {
  provider?: string;
  model?: string;
  tools?: { allow?: string[]; deny?: string[] };
  prompt?: string;
};

export type SpawnOpts = {
  cwd?: string;
  extensions?: string[];
  env?: Record<string, string>;
  onSpawn?: (taskManagerId: string) => void;
};

export type AgentResult = {
  ok: boolean;
  output: string;
  errors: string;
  filesChanged: string[];
  exitCode: number;
};

export type SkillResult = {
  stdout: string;
  exitCode: number;
};

export type BackgroundSkillHandle = {
  stdout: () => string;
  stderr: () => string;
  done: Promise<SkillResult>;
};

export type DisguiseConfig = {
  model?: string;
  context?: string[];
  writePaths?: string[];
  tools?: Record<string, ToolDef>;
  writeHooks?: Record<string, (path: string) => Effect[]>;
  state?: () => Record<string, unknown>;
  dispatchOpts?: DispatchOpts;
};

export type DispatchOpts = {
  maxQueueDepth?: number;
  minDelayMs?: number;
  concurrency?: number;
};

export type ToolDef = {
  description: string;
  params: Record<string, ToolParam>;
  /** When true, the tool ignores Pi's abort signal (for tools that block on user interaction) */
  longRunning?: boolean;
  /** When provided, tool is only callable when this returns true. Otherwise returns unavailableMessage or a default error with the list of available tools. */
  available?: (state: Record<string, unknown>) => boolean;
  unavailableMessage?: string;
  execute: (params: Record<string, unknown>, ctx: WorkflowContext) => Promise<unknown>;
};

export type ToolParam = {
  type: string;
  description: string;
  required?: boolean;
};

export type MessageOpts = {
  display?: boolean;
  triggerTurn?: boolean;
};

export type InputOpts = {
  timeout?: number;
  defaultValue?: string;
  maxRetries?: number;
  validate?: (raw: string) => string | null;
};

export type ToolRegistration = {
  name: string;
  description: string;
  params: Record<string, ToolParam>;
};

export interface RuntimeHost {
  registerTool(spec: ToolRegistration): void;
  removeTool(name: string): void;
  setActiveTools(tools: string[]): void;
  getActiveTools(): string[];
  sendMessage(content: string, opts?: MessageOpts): void;
  notifyUser(message: string, type?: "info" | "warning" | "error"): void;
  appendSystemPrompt(content: string): void;
  spawnAgent(config: AgentConfig, prompt: string, opts?: SpawnOpts): Promise<AgentResult>;
  discoverSkills(skillsDir: string): SkillRegistry;
  runSkill(name: string, args: string[]): Promise<SkillResult>;
  runSkillBackground(name: string, args: string[]): BackgroundSkillHandle | null;
  openUrl(url: string): void;
  onToolCall(handler: (event: ToolCallEvent) => Promise<ToolCallResponse>): void;
  cwd: string;
  piDir: string;
  input(prompt: string, opts?: InputOpts): Promise<string>;
  select(title: string, options: string[]): Promise<string | undefined>;
  setWidget(key: string, lines: string[] | undefined): void;
}

export type SkillEntry = {
  name: string;
  command: string;
  skillPath: string;
};

export type SkillRegistry = Map<string, SkillEntry>;

export type ToolCallEvent = {
  toolName: string;
  input: Record<string, unknown>;
};

export type ToolCallResponse = {
  block: boolean;
  reason?: string;
};

export type DisguiseExport = Record<string, DisguiseConfig>;
