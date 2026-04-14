# maintainability

Policy that measures Rust maintainability (cyclomatic complexity, Halstead volume, LOC), reports reward, and searches commit space with MCTS.

## Build

From the project root:

```bash
cargo make wasm
```

Composed component output:

- `mitb-examples/target/maintainability.composed.component.wasm`

## Run

```bash
mitb mitb-examples/target/maintainability.composed.component.wasm codex
```

To exclude Rust files under specific paths from maintainability scoring, set
`MITB_MAINTAINABILITY_IGNORED_PATHS` to a comma-separated list of repository
relative paths (directories or files). Matching is recursive for directories.

```bash
MITB_MAINTAINABILITY_IGNORED_PATHS="target,mitb-examples,vendor/generated.rs" \
  mitb mitb-examples/target/maintainability.composed.component.wasm codex
```

Notes:

- Run in a Git repository; this policy creates commits and switches to selected commits/branches.
