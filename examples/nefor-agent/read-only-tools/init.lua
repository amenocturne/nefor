-- examples/nefor-agent/read-only-tools — config composition for the read-only tool set.
--
-- The mechanism (list_dir / optional search_text / python-read / instructions /
-- discover_instruction_files handlers plus the advertise/dispatch
-- plumbing) lives in `libs.read-only-tools`. This file is the config's
-- opinion layer: it declares which base tools the source advertises via
-- `include`. Base tools are opt-in, so the starter names its full set here.
-- A downstream config picks its own subset and adds typed tools via
-- `extra_tools = { { schema = ..., handler = function(args, emit) ... } }`.
return require("libs.read-only-tools").build {
  include = {
    "list_dir",
    "python-read",
    "instructions",
    "discover_instruction_files",
  },
}
