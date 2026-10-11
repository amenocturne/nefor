# MAG errors and recovery

All MAG failures happen during compilation: parsing, name resolution, type checking, and evaluation all happen before an artifact exists. Executing an artifact is the embedding application's job, so its runtime failures are documented by that application, not here.

See [Language reference](language.md) for valid forms and [Compiling MAG](compiling.md) for the standalone compiler.

## Diagnostics

A failed compilation produces one structured diagnostic with a `code`, `stage`, `message`, optional `path`, and syntax detail where the parser has it. The standalone compiler exits nonzero, writes that diagnostic to stderr, and produces no stdout artifact.

## Compile-time language errors

Parsing, name resolution, and type checking reject a program before evaluation. Typical causes:

- using a Lisp `(require "...")` form in a `.mag` file instead of a supported direct `import module.identity`, `import module.identity.{...}`, or `import module.identity as alias` declaration;
- referencing an undeclared name or type, or a qualified name without a matching direct import;
- an import collision between namespace roots, aliases, builtins, local declarations, and opened or selected exports. Values of one name and different types are overloads, not a collision; two values of one name and the same type report `import collision at 'name'; candidates: ...; same type T; repairs: ...`;
- passing a value that does not conform to a declared record, function parameter, or annotation;
- a non-exhaustive, repeated, or foreign `match` case;
- comparing values with `=` when they contain functions, type witnesses or descriptors, packed compiler values, or artifacts;
- returning something other than an artifact from the entry program.

## ADT construction

Nominal ADTs never accept a payload as though it were the owner value. Construct the owner explicitly with `Owner.Constructor(payload)`, for example `Result<String, Int>.Ok(42)`. The compiler reports both source and target types for an invalid refinement, but does not currently attach a source span to this evaluation-time diagnostic.

Construction is checked before evaluation. The checker reports:

- `constructor C is not a member of T; available constructors: A, B` when the owner is an ADT that does not declare the constructor (the same message covers a foreign `match` case);
- `T is not an ADT and has no constructor C` when the owner is a record, alias target, or built-in type;
- `cannot infer type argument P of T from the payload or the expected type; write T<...>.C(...) with explicit type arguments` when a generic owner is written without arguments and neither the payload nor an expected type determines parameter `P`, as in an unannotated `let absent = Option.None`;
- `constructor T.C has no payload and takes no argument list; write T.C` when an alternative declared as a bare name is given an argument list, including `()` and `(nil)`;
- `constructor T.C takes a payload of type P; write T.C(...)` when a constructor with a payload is named without arguments;
- `constructor T.C has no payload to bind; write case C =>` and `constructor T.C has a payload of type P; write case C(name) =>` when a `match` case uses the other constructor kind's form;
- `constructor C() declares no payload; write it as the bare name C` for the removed `C()` declaration form;
- `constructor T.C takes a payload of type P, got N arguments` when the argument count cannot form the payload, and an ordinary type mismatch when the payload has the wrong type;
- `constructor T.C takes N fields (a, b), got M arguments` when positional arguments do not match an inline record payload, `field a of constructor T.C: expected A, got B` when a field's value has the wrong type, and `constructor arguments must be either all positional or all named` when the two forms are mixed;
- `duplicate constructor C in T` when one ADT declares a constructor name twice;
- `unresolved symbol: C` for a bare `C(...)`, because constructors are not module-scope names. Write `T.C(...)`.

A construction whose owner differs from an annotation, such as `let value: Other = T.C(1)`, is a type error at the binding.

## Library-reported errors

Evaluation may also fail because a library rejects a value, for example through `fail([...])`. The diagnostic message then comes from that library, and its documentation describes the rule and the repair. The compiler only reports that evaluation stopped.

## Correction sequence

1. Read the diagnostic's `stage` and `code` to see whether parsing, checking, or evaluation failed.
2. Use its `path`, syntax detail, and message to find the offending declaration or value.
3. Change the source; recompiling an unchanged source tree produces the same diagnostic.
4. Compile again and inspect the new artifact.
