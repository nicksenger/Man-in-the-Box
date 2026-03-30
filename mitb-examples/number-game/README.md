# number-game

Policy that asks for guesses in `<guess>...</guess>` format and reports reward based on closeness.

## Build

From the project root:

```bash
cargo make wasm
```

Component output:

- `mitb-examples/target/number_game.component.wasm`

## Run

```bash
mitb mitb-examples/target/number_game.component.wasm codex
```

Optional:

- Set `SECRET_NUMBER` to force a deterministic target number.
