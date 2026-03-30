# always-wait

Minimal policy that always returns `Action::Wait`.

## Build

From the project root:

```bash
cargo make wasm
```

Component output:

- `mitb-examples/target/always_wait.component.wasm`

## Run

```bash
mitb mitb-examples/target/always_wait.component.wasm codex
```
