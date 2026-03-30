mitb_sdk::policy_prelude!("number-game");

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

impl Default for NumberGame {
    fn default() -> Self {
        Self {
            state: State::Initial,
            secret_number: bindings::wasi::random::random::get_random_u64() as u32,
        }
    }
}

impl Policy for NumberGame {
    async fn act(&mut self, pty_contents: String) -> ActionResult {
        match &mut self.state {
            state @ State::Initial => {
                log::info!("The goal is: {}", self.secret_number);
                *state = State::Playing {
                    previous_guess: None,
                    last_guess_text: String::new(),
                };
                prompt!(
                    "I'm thinking of a 32-bit unsigned integer. Can you guess what it is? Reply in the format <guess>N</guess> where N is your guess."
                )
            }
            State::Finished => {
                *self = Default::default();
                prompt!("Want to play again? I have a new number in mind!")
            }
            State::Playing {
                previous_guess,
                last_guess_text,
            } => {
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

                if guess == self.secret_number {
                    report_reward!(1.0);
                    self.state = State::Finished;
                    return Ok(Action::Wait);
                }

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
                report_reward!(reward);

                prompt!(reply)
            }
        }
    }
}

bindings::export_policy!(NumberGame);
