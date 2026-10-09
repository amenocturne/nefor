# MAG errors and recovery

All MAG failures happen during compilation: parsing, name resolution, type checking, and evaluation all happen before an artifact exists. Executing an artifact is the embedding application's job, so its runtime failures are documented by that application, not here.

See [Language reference](language.md) for valid forms and [Compiling MAG](compiling.md) for the standalone compiler.

## Diagnostics

A failed compilation produces one structured diagnostic with a `code`, `stage`, `message`, optional `path`, and syntax detail where the parser has it. The standalone compiler exits nonzero, writes that diagnostic to stderr, and produces no stdout artifact.

## Compile-time language errors

Parsing, name resolution, and type checking reject a program before evaluation. Typical causes:

- using a Lisp `(require "...")` form in a `.mag` file instead of a supported direct `import module.identity`, `import module.identity.{...}`, or `import module.identity as alias` declaration;
- referencing an undeclared name or type, or a qualified name without a matching direct import;
- an import collision between namespace roots, aliases, builtins, local declarations, and opened or selected exports;
- passing a value that does not conform to a declared record, function parameter, or annotation;
- a non-exhaustive, repeated, or foreign `match` case;
- comparing values with `=` when they contain functions, type witnesses or descriptors, packed compiler values, or artifacts;
- returning something other than an artifact from the entry program.

## ADT construction

Nominal ADTs never accept a payload as though it were the owner value. Construct the owner explicitly with `Owner.Constructor(payload)`, for example `Result<String, Int>.Ok(42)`. The compiler reports both source and target types for an invalid refinement, but does not currently attach a source span to this evaluation-time diagnostic.

## Library-reported errors

Evaluation may also fail because a library rejects a value, for example through `fail([...])`. The diagnostic message then comes from that library, and its documentation describes the rule and the repair. The compiler only reports that evaluation stopped.

## Correction sequence

1. Read the diagnostic's `stage` and `code` to see whether parsing, checking, or evaluation failed.
2. Use its `path`, syntax detail, and message to find the offending declaration or value.
3. Change the source; recompiling an unchanged source tree produces the same diagnostic.
4. Compile again and inspect the new artifact.
