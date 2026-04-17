import { readFileSync } from "fs";
import { join, dirname } from "path";
import { parse } from "yaml";
import { fileURLToPath } from "url";

export type FlavourConfig = {
  provider: string;
  models: {
    orchestrator: string;
    worker: string;
    reviewer: string;
    explorer: string;
    tester: string;
    promptEngineer: string;
  };
};

const configDir = (() => {
  try {
    return dirname(fileURLToPath(import.meta.url));
  } catch {
    // Fallback for runtimes that don't support import.meta.url (e.g. CJS loaders)
    return join(process.cwd(), ".pi", "config");
  }
})();

export const loadConfig = (): FlavourConfig => {
  const configPath = join(configDir, "config.yaml");
  const raw = readFileSync(configPath, "utf-8");
  return parse(raw) as FlavourConfig;
};

const config: FlavourConfig = loadConfig();
export default config;
