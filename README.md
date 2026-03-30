# Man in the Box

Man in the Box (`mitb`) runs a WebAssembly (WASI) reward-policy against a PTY-hosted agentic terminal interface with BYOV (bring your own video) overlay. Direct user interaction with the terminal is blocked to prevent the user from interfering with the _Man in the Box_.

Watch the _Man in the Box_ have Codex 5.3 improve the Maintainability Index of Codex CLI after a grueling game of guess-the-number (fast-forwarded):

https://github.com/user-attachments/assets/2a42199b-8d85-4dd6-87ab-be29e51a8b55

Please note that the _Man in the Box_ is fundamentally a PTY automation utility, **not** a **sandbox** or a **throttle**. You will need to handle safety and provisioning on your own.

## Installation

Install the Rust build tool, Cargo:

```
curl https://sh.rustup.rs -sSf | sh
```

Then install `mitb` from the project root:

```
cargo install --path .
```

To build the example policies, install `cargo-make` and `wasm-tools`:

```
cargo install --locked wasm-tools cargo-make
```

Then build the examples from the project root:

```
cargo make wasm
```

## Usage

```bash
mitb /path/to/my/policy.wasm claude
```

Or provide more complex commands with:

```bash
mitb /path/to/my/policy.wasm --cmd "codex --sandbox workspace-write"
```

Enable remote vibe checking by setting both signaling env vars explicitly:

```bash
MITB_SERVER_ADDR=ws://127.0.0.1:3000/ws MITB_SECRET_CODE=change-me \
  mitb /path/to/my/policy.wasm claude
```

If you want to use my public signaling server and `lobby` room:

```bash
MITB_SERVER_ADDR=wss://mitb.nsenger.com/ws MITB_SECRET_CODE=lobby \
  mitb /path/to/my/policy.wasm claude
```

## Policy Definition

A `mitb` policy is a WASM component that observes terminal (PTY) state and decides what to do next:

- `Action::Wait`: do nothing this tick.
- `Action::Perturb(...)`: send key/text inputs.
- `prompt!(...)`: shorthand for sending text + Enter to the agent terminal.

Most policies use `mitb_sdk::policy_prelude!`, implement `Policy::act`, and export with `bindings::export_policy!(MyPolicy)`.

A nice way to demonstrate the utility is by having the agent perform a task which it is exceedingly unlikely to one-shot, like guessing a random number: 

```rust
mitb_sdk::policy_prelude!("number-game");

// Policies can be stateful.
#[derive(Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Initial,
    Playing {
        previous_guess: Option<u32>,
        last_guess_text: String,
    },
    Finished,
}

struct NumberGame {
    state: State,
    secret_number: u32,
}

// Pick a random u32 for this instance of the policy.
impl Default for NumberGame {
    fn default() -> Self {
        Self {
            state: State::Initial,
            secret_number: bindings::wasi::random::random::get_random_u64() as u32,
        }
    }
}

impl Policy for NumberGame {
    // This method is called whenever the agent is determined to be idle.
    // For control over idle-detection, the `GuestSession` trait may
    // be implemented directly.
    async fn act(&mut self, pty_contents: String) -> ActionResult {
        match &mut self.state {
            // When the agent first starts, we introduce the game.
            state @ State::Initial => {
                *state = State::Playing {
                    previous_guess: None,
                    last_guess_text: String::new(),
                };
                prompt!(
                    "I'm thinking of a 32-bit unsigned integer. Can you guess what it is? Reply in the format <guess>N</guess> where N is your guess."
                )
            }
            // After the agent has guessed correctly, we can make them play again and see if they learned a good strategy!
            State::Finished => {
                *self = Default::default();
                prompt!("Want to play again? I have a new number in mind!")
            }
            // Run the main game logic.
            State::Playing {
                previous_guess,
                last_guess_text,
            } => {
                // Extract the agent's guess from the raw PTY output.
                let Some(guess_text) =
                    regex_capture!(&pty_contents, r"(?s)<guess>\s*([^<]+?)\s*</guess>")?
                else {
                    return Ok(Action::Wait);
                };

                if last_guess_text.as_str() == guess_text {
                    return Ok(Action::Wait);
                }

                let guess = match guess_text.parse::<u32>() {
                    Ok(value) => value,
                    Err(_) => {
                        return prompt!(
                            "I couldn't parse your guess. Use format <guess>N</guess> with a u32 value."
                        );
                    }
                };
                *last_guess_text = guess_text;

                // Transition to the finished state if applicable, and report max reward.
                if guess == self.secret_number {
                    report_reward!(1.0);
                    self.state = State::Finished;
                    return Ok(Action::Wait);
                }

                // We tell the agent the direction to move in, but not how far.
                let reply = match *previous_guess {
                    None => "Nope, guess again.".to_string(),
                    Some(previous_guess) => {
                        let direction = if guess > self.secret_number {
                            "high"
                        } else {
                            "low"
                        };
                        let current_distance = u32::abs_diff(guess, self.secret_number);
                        let previous_distance = u32::abs_diff(previous_guess, self.secret_number);

                        if current_distance < previous_distance {
                            format!(
                                "Nope, you're too {direction}, that's closer than {previous_guess}."
                            )
                        } else if current_distance > previous_distance {
                            format!(
                                "Nope, you're too {direction}, that's further than {previous_guess}."
                            )
                        } else {
                            "Nope, guess again.".to_string()
                        }
                    }
                };
                *previous_guess = Some(guess);

                let normalized_distance =
                    (u32::abs_diff(guess, self.secret_number) as f64) / (u32::MAX as f64);
                let reward = (1.0 - normalized_distance).clamp(0.0, 1.0);

                // Report the reward so we can track the agent's progress remotely.
                report_reward!(reward);

                prompt!(reply)
            }
        }
    }
}

bindings::export_policy!(NumberGame);
```

The full interface definition is in `./mitb-wit/wit/world.wit`. For a more realistic use-case, see the [Maintainability Example](./mitb-examples/maintainability/README.md), which improves a Rust project's maintainability index.

### Reward convention (normalized)

Rewards are required to be finite and normalized to `[0.0, 1.0]`.

- `0.0`: worst observed outcome for your objective.
- `1.0`: best observed outcome.
- intermediate values: graded progress.

`mitb-host` validates this range before forwarding reward reports, so clamp/normalize in-policy before calling `report_reward!`.

### Why this is useful

The policy puts a programmable gate between yourself and the agent:

- prompt until your conditions are met
- report rewards whenever measurable progress occurs
- check the progress or terminate the agent remotely

This shifts the interaction from constant manual nudging to sparse, objective-driven control.

## Advanced Features

### Vibe Checking and Killing

Users may check and kill the vibe remotely by setting both `MITB_SERVER_ADDR` (to a running `mitb-server` instance) and `MITB_SECRET_CODE` (to your shared room secret).

This forwards the reported `reward`s from your policy over [WebRTC](https://webrtc.org/) to any interested parties who know your secret code. This is secured by [PAKE](https://en.wikipedia.org/wiki/Password-authenticated_key_agreement) ([opaque-ke](https://github.com/facebook/opaque-ke)), so the server also should not know your code.

A simple web UI facilitating vibe checking and killing is served from the `mitb-server` root (e.g. [https://mitb.nsenger.com](https://mitb.nsenger.com)).

![Web UI](https://github.com/user-attachments/assets/a7374dde-d7ee-4c13-bcc4-f6c86778bf7c)

Note that a full control interface is intentionally **not** exposed.

### BYOV Overlay

If `~/.mitb/mitb.mkv` exists, `mitb` will decode and overlay AV1 video directly into the PTY viewer [iced](https://github.com/iced-rs/iced) render loop while playing Opus audio in the background. AV1 and Opus are the only supported codecs. If the `~/.mitb/mitb.mkv` file does not exist, no overlay will be rendered.

I recommend using the 1991 single "Man in the Box" by Alice in Chains, since that's what this project was named after. Here is a command to download it from the Internet Archive, convert it, and place it in the expected location:

```bash
mkdir -p "$HOME/.mitb" && \
  ffmpeg -y -i input.mp4 \
    -c:v libsvtav1 \
    -c:a libopus \
    "$HOME/.mitb/mitb.mkv"
```

If you like the song, download it on Spotify, or better yet buy an album.

R.I.P, Layne.
