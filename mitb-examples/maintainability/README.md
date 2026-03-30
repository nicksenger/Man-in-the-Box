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

Notes:

- Run in a Git repository; this policy creates commits and switches to selected commits/branches.
