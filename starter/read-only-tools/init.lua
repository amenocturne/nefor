-- starter/read-only-tools/init.lua — team read-only tool surface.
--
-- The mechanism (list_dir / search_text / discover_instruction_files / skill
-- handlers plus advertise/dispatch) lives in libs.read-only-tools. This file is
-- the config's opinion layer: it names the base tools the team advertises.
-- `skill` loads the team CLI workflows (dp for Jira, confluence for wiki) from
-- <config>/skills/<name>/skill.md. Workspace AGENTS.md/CLAUDE.md awareness comes
-- two ways: passive reminders fire off list_dir/search_text, and
-- discover_instruction_files lets an agent enumerate them on demand. The
-- `instructions` tool (reads <config>/instructions/*.md) is NOT included — the
-- team has no config instruction docs. python-read is deliberately omitted.
return require("libs.read-only-tools").build {
  include = {
    "list_dir",
    "search_text",
    "discover_instruction_files",
    "skill",
  },
}
