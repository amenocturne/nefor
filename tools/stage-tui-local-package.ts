// Explicit test-only local package override: production source/pins stay intact.
import { cpSync, mkdirSync, readFileSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [repo, suite, binary] = Bun.argv.slice(2);
if (!repo || !suite || !binary) throw new Error("repo, suite and local da executable required");
const config = join(suite, "config");
const packageRoot = join(suite, "local-da");
mkdirSync(join(packageRoot, "bin"), { recursive: true });
symlinkSync(realpathSync(binary), join(packageRoot, "bin", "da"));
cpSync(join(repo, "examples/nefor-agent"), config, { recursive: true });
const init = join(config, "init.lua");
const source = readFileSync(init, "utf8");
const marker = 'local da_package = require("libs.tool-validator.da")';
if (source.split(marker).length !== 2) throw new Error("local package fixture seam missing");
writeFileSync(init, source.replace(marker, `${marker}
local release_package = da_package.release_package
da_package.release_package = function(opts)
  local package = release_package(opts)
  package.archive = nil
  package.dir = ${JSON.stringify(packageRoot)}
  return package
end
`));
for (const name of ["starter-initial-prompt", "starter-mock-smoke", "starter-mock-multi-turn", "starter-permission-commands", "starter-sidebar-overflow", "starter-interrupt-recovery"]) {
  const scenario = JSON.parse(readFileSync(join(repo, "tests/tui", `${name}.json`), "utf8"));
  scenario.command.argv = scenario.command.argv.map((arg: string) => arg.replaceAll("NEFOR_CONFIG_DIR=$PWD/examples/nefor-agent", `NEFOR_CONFIG_DIR=${JSON.stringify(config)}`));
  writeFileSync(join(suite, `${name}.json`), JSON.stringify(scenario));
}
