# mitb-examples

Standalone policy examples for `mitb`.

## Policies

- `always-wait`: never sends input, always returns `wait`.
- `number-game`: plays a number guessing game via `<guess>...</guess>` tags.
- `maintainability`: scores Rust maintainability and explores commits with MCTS.
- `git-responder`: addresses unresolved Git MR/PR comments.

## Build

Build all example WASM artifacts:

```bash
cargo build --manifest-path mitb-examples/Cargo.toml --release --target wasm32-unknown-unknown
```

Create component artifacts (including Tree-sitter composition for maintainability):

```bash
cargo make wasm
```

Output components are written to `mitb-examples/target/`.
