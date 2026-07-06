-- starter/read-only-tools — config composition for the read-only tool set.
--
-- The mechanism (list_dir / search_text / python-read / instructions /
-- discover_instruction_files handlers plus the advertise/dispatch
-- plumbing) lives in `libs.read-only-tools`. This file is the config's
-- opinion layer: it declares which tools the source advertises. Starter
-- ships only the base set — no extras — so it builds with an empty
-- registration. A downstream config adds typed tools here via
-- `extra_tools = { { schema = ..., handler = function(args, emit) ... } }`.
return require("libs.read-only-tools").build()
