use std::collections::BTreeSet;

use crate::bindings::mitb::host::types::{Input, Key};
use crate::{_sdk_log, POLICY_LOG_SCOPE};

use super::{Action, ActionResult, GitConfig};

pub(crate) const ENV_GIT_DRIP_SOURCE: &str = "MITB_GIT_DRIP_SOURCE";
pub(crate) const ENV_GIT_DRIP_TARGET: &str = "MITB_GIT_DRIP_TARGET";
pub(crate) const ENV_GIT_DRIP_HINT: &str = "MITB_GIT_DRIP_HINT";
const MERGE_STATUS_PREFIX: &str = "__MITB_GIT_MERGE_STATUS=";

/// Tracks the current pipeline step for the drip emitter state machine.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[allow(dead_code)]
pub(crate) enum DripEmitterStep {
    #[default]
    Init,
    Apply,
    Adapt,
    Push,
}

/// The current state of the drip emitter pipeline.
#[derive(Clone, Debug)]
struct DripEmitterState {
    step: DripEmitterStep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeOutcome {
    Clean,
    Conflict,
}

impl Default for DripEmitterState {
    fn default() -> Self {
        Self {
            step: DripEmitterStep::default(),
        }
    }
}

/// Drives a four-step pipeline: select a thread, apply source changes,
/// adapt the target branch, then push the target branch back to the remote.
///
/// The first three steps return prompts while the last one commits / pushes.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct DripEmitter {
    source: String,
    target: String,
    hint: String,
    thread_id: String,
    comment_id: u64,
    state: DripEmitterState,
    addressed_discussions: BTreeSet<String>,
}

impl DripEmitter {
    /// Create a new DripEmitter for the given source, target, and hint branches.
    pub(crate) fn new(source: String, target: String, hint: String) -> Self {
        Self {
            source,
            target,
            hint,
            thread_id: String::new(),
            comment_id: 0,
            state: DripEmitterState::default(),
            addressed_discussions: BTreeSet::new(),
        }
    }

    /// Return the current pipeline step.
    #[allow(dead_code)]
    pub(crate) fn current_step(&self) -> DripEmitterStep {
        self.state.step
    }

    /// Set the thread and comment context for commit messages.
    pub(crate) fn set_thread_context(&mut self, thread_id: String, comment_id: u64) {
        self.thread_id = thread_id;
        self.comment_id = comment_id;
    }

    /// Build a commit message that references the active thread and comment.
    fn commit_message(&self) -> String {
        format!(
            "chore: Apply source changes for drip emission (thread={}, comment={})",
            self.thread_id, self.comment_id
        )
    }

    /// Apply policy changes on the source branch.
    pub(crate) async fn apply_source_changes(&mut self) -> ActionResult {
        let _ = super::run_process("git", vec!["add".to_string(), "-A".to_string()]).await?;
        super::run_process(
            "git",
            vec![
                "commit".to_string(),
                "-m".to_string(),
                self.commit_message(),
            ],
        )
        .await?;
        super::run_process("git", vec!["checkout".to_string(), self.target.clone()]).await?;
        Ok(prompt_action(format!(
            "The branch has been changed to {}, which contains only the subset of changes from the prior branch {} relevant to {}. Please apply the subset of changes made in response to comment {} relevant to {} to the current branch ({})",
            self.target, self.source, self.hint, self.comment_id, self.hint, self.target
        )))
    }

    /// Adapt the target branch to include changes relevant to the hint.
    pub(crate) async fn adapt_target_branch(&mut self) -> ActionResult {
        let _ = super::run_process("git", vec!["add".to_string(), "-A".to_string()]).await?;
        super::run_process(
            "git",
            vec![
                "commit".to_string(),
                "-m".to_string(),
                format!("chore: Address comment {}", self.comment_id),
            ],
        )
        .await?;
        super::run_process("git", vec!["checkout".to_string(), self.source.clone()]).await?;
        match self.merge_target_branch().await? {
            MergeOutcome::Clean => Ok(Action::Wait),
            MergeOutcome::Conflict => {
                self.accept_source_on_conflict().await?;
                Ok(Action::Wait)
            }
        }
    }

    /// Push the target branch to the remote.
    pub(crate) async fn push_target_branch(&mut self) -> ActionResult {
        super::run_process("git", vec!["checkout".to_string(), self.target.clone()]).await?;
        super::run_process(
            "git",
            vec![
                "push".to_string(),
                "origin".to_string(),
                self.target.clone(),
            ],
        )
        .await?;
        super::run_process("git", vec!["checkout".to_string(), self.source.clone()]).await?;
        super::log::info!(
            "Target branch ({}) has been pushed to origin. All done.",
            self.target
        );
        Ok(Action::Wait)
    }

    async fn merge_target_branch(&self) -> Result<MergeOutcome, String> {
        let output = super::run_process(
            "bash",
            vec![
                "-lc".to_string(),
                "set +e; git merge \"$1\" 2>&1; status=$?; printf '\\n__MITB_GIT_MERGE_STATUS=%s\\n' \"$status\"; exit 0".to_string(),
                "mitb-drip-merge".to_string(),
                self.target.clone(),
            ],
        )
        .await?;
        let output = String::from_utf8(output)
            .map_err(|error| format!("merge output was not valid utf-8: {error}"))?;
        let merge_status = parse_merge_status(output.as_str())?;

        match merge_status {
            0 => Ok(MergeOutcome::Clean),
            1 => Ok(MergeOutcome::Conflict),
            code => {
                let merge_output = output
                    .lines()
                    .filter(|line| !line.starts_with(MERGE_STATUS_PREFIX))
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(format!(
                    "git merge `{}` exited with unexpected status {}: {}",
                    self.target,
                    code,
                    mitb_sdk::truncate(merge_output.trim(), 2000)
                ))
            }
        }
    }

    async fn accept_source_on_conflict(&self) -> Result<(), String> {
        super::log::info!(
            "Merge conflict while merging {} into {}. Accepting source side and continuing.",
            self.target,
            self.source
        );
        super::run_process("git", vec!["merge".to_string(), "--abort".to_string()]).await?;
        super::run_process(
            "git",
            vec![
                "merge".to_string(),
                "-s".to_string(),
                "ours".to_string(),
                self.target.clone(),
            ],
        )
        .await?;
        Ok(())
    }

    /// Select the next unresolved thread and return it as a prompt.
    /// Advances the pipeline to the Apply step.
    async fn select_next_thread(&mut self, config: &GitConfig) -> ActionResult {
        let unresolved_threads = super::list_unresolved_threads(config).await?;
        for unresolved in &unresolved_threads {
            let _ = self
                .addressed_discussions
                .remove(unresolved.discussion_id.as_str());
        }

        let reward =
            super::percent_addressed(self.addressed_discussions.len(), unresolved_threads.len());
        super::report_reward(reward).await?;

        if unresolved_threads.is_empty() {
            return Ok(Action::Wait);
        }

        let thread = unresolved_threads
            .into_iter()
            .next()
            .ok_or_else(|| String::from("failed selecting unresolved discussion"))?;
        let _ = super::terminal_head().await;
        self.set_thread_context(thread.discussion_id.clone(), thread.comment_id);
        self.addressed_discussions
            .insert(thread.discussion_id.clone());
        super::mark_thread_in_progress(config, thread.discussion_id.as_str(), thread.comment_id)
            .await;

        super::reply_artifact().clear().await;
        let prompt = super::build_thread_prompt(
            thread.summary.as_str(),
            thread.comment_id,
            config.responder_mode,
        );

        if let Some(s) = config.clear_cmd.as_ref() {
            Ok(Action::Perturb(vec![
                Input::Text(s.to_string()),
                Input::Key(Key::Enter),
                Input::Key(Key::Enter),
                Input::Text(prompt),
                Input::Key(Key::Enter),
            ]))
        } else {
            Ok(prompt_action(prompt))
        }
    }

    /// Run the full drip-emission pipeline: select a thread, apply changes, adapt, then push.
    pub(crate) async fn run(&mut self, config: &GitConfig) -> ActionResult {
        match self.state.step {
            DripEmitterStep::Init => {
                let result = self.select_next_thread(config).await?;
                if matches!(result, Action::Wait) {
                    return Ok(Action::Wait);
                }
                self.state.step = DripEmitterStep::Apply;
                Ok(result)
            }
            DripEmitterStep::Apply => {
                self.state.step = DripEmitterStep::Adapt;
                self.apply_source_changes().await
            }
            DripEmitterStep::Adapt => {
                self.state.step = DripEmitterStep::Push;
                self.adapt_target_branch().await
            }
            DripEmitterStep::Push => {
                self.state.step = DripEmitterStep::Init;
                self.push_target_branch().await?;
                let commit_sha = super::read_trimmed_stdout(
                    "git",
                    vec!["rev-parse".to_string(), self.target.clone()],
                )
                .await?;
                let reply = super::reply_artifact().read().await?;
                let reply_body =
                    super::addressed_commit_reply(commit_sha.as_str(), reply.as_deref());
                super::post_discussion_reply(config, self.thread_id.as_str(), reply_body.as_str())
                    .await?;
                super::resolve_discussion(config, self.thread_id.as_str(), self.comment_id).await?;
                super::mark_thread_complete(config, self.thread_id.as_str(), self.comment_id).await;
                super::log::info!(
                    "Resolved discussion {} after successful drip push with commit {}.",
                    self.thread_id,
                    commit_sha
                );
                Ok(Action::Wait)
            }
        }
    }
}

pub(crate) fn parse_from_environment() -> Option<DripEmitter> {
    let source = super::optional_env_any(&[ENV_GIT_DRIP_SOURCE])?;
    let target = super::optional_env_any(&[ENV_GIT_DRIP_TARGET])?;
    let hint = super::optional_env_any(&[ENV_GIT_DRIP_HINT])?;
    Some(DripEmitter::new(source, target, hint))
}

fn prompt_action(message: String) -> Action {
    Action::Perturb(vec![Input::Text(message), Input::Key(Key::Enter)])
}

fn parse_merge_status(output: &str) -> Result<i32, String> {
    let status = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(MERGE_STATUS_PREFIX))
        .ok_or_else(|| String::from("merge status marker was not found in command output"))?;
    status
        .trim()
        .parse::<i32>()
        .map_err(|error| format!("failed parsing merge status marker `{status}`: {error}"))
}

#[cfg(test)]
mod tests {
    use super::parse_merge_status;

    #[test]
    fn parse_merge_status_reads_marker_from_tail_output() {
        let output = "Auto-merging src/lib.rs\nCONFLICT (content): Merge conflict in src/lib.rs\n\n__MITB_GIT_MERGE_STATUS=1\n";
        assert_eq!(parse_merge_status(output).unwrap(), 1);
    }
}
