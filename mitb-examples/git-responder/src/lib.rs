//! Non-drip lifecycle for the git responder policy:
//!
//! 1. `act()` loads config, then either resumes `active_thread` or selects the next unresolved thread.
//! 2. Selection claims an optional filesystem lease, marks the thread in-progress, clears `.mitb-reply`,
//!    and prompts the model with thread context.
//! 3. Finalization captures reply text and evaluates repository changes; when edits exist, optional
//!    verification runs before creating a commit for the triggering comment.
//! 4. Integration rebases/cherry-picks the pending commit onto the target branch and pushes; failures
//!    requeue the thread with a follow-up prompt while preserving state in `NonDripState`.
//! 5. Completion posts the reviewer-facing reply, resolves the discussion, applies completion reactions,
//!    releases the lease, and restores the target branch when running in parallel mode.
//!
mitb_sdk::policy_prelude!("git-responder");

use core::time::Duration;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::bindings::mitb::host::types::{Input, Key};
use drip_emitter::DripEmitter;

mod drip_emitter;
mod env;
mod providers;
mod reply;
mod thread_context;

const ENV_GIT_TOKEN: &str = "MITB_GIT_TOKEN";
const ENV_GIT_PR_URL: &str = "MITB_GIT_PR_URL";
const ENV_VERIFICATION_COMMAND: &str = "MITB_VERIFICATION_COMMAND";
const ENV_VERIFICATION_TIMEOUT_SECONDS: &str = "MITB_VERIFICATION_TIMEOUT_SECONDS";
const ENV_READ_ONLY: &str = "MITB_READ_ONLY";
const ENV_CLEAR_COMMAND: &str = "MITB_CLEAR_COMMAND";
// Comma-delimited list of allowed Git usernames for thread filtering
const ENV_GIT_ALLOWED_USERS: &str = "MITB_GIT_ALLOWED_USERS";
const ENV_GIT_PARALLEL_FILESYSTEM_LEASES: &str = "MITB_GIT_PARALLEL_FILESYSTEM_LEASES";
const ENV_GIT_LEASE_ROOT: &str = "MITB_GIT_LEASE_ROOT";
const ENV_GIT_LEASE_TTL_SECONDS: &str = "MITB_GIT_LEASE_TTL_SECONDS";
const ENV_GIT_LEASE_HEARTBEAT_SECONDS: &str = "MITB_GIT_LEASE_HEARTBEAT_SECONDS";
const ENV_GIT_WORKER_ID: &str = "MITB_GIT_WORKER_ID";
const ENV_SHARED_ROOT: &str = "MITB_SHARED_ROOT";

const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_THREAD_SUMMARY_BYTES: usize = 16 * 1024;
const SNIPPET_CONTEXT_LINES: usize = 4;
const MAX_SNIPPET_BYTES: usize = 4 * 1024;
const MAX_VERIFICATION_OUTPUT_BYTES: usize = 16 * 1024;
const GIT_REPLY_PREAMBLE: &str = "[This is an automated reply from the **_Man in the Box_**]\n\n";
const REPLY_ARTIFACT_PATH: &str = ".mitb-reply";
const REACTION_THREAD_PICKED_UP: [&str; 2] = ["eye", "takeout_box"];
const REACTION_THREAD_COMPLETE: &str = "ballot_box_with_check";
const VERIFICATION_STREAM_DRAIN_GRACE_NS: u64 = 5_000_000_000;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_BETWEEN_BYTES_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(8 * 60);
const DEFAULT_LEASE_ROOT: &str = "mitb-shared/leases";
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_INTEGRATION_PUSH_RETRIES: u32 = 3;

#[derive(Default)]
struct GitResponder {
    config: Option<GitConfig>,
    addressed_discussions: BTreeSet<String>,
    active_thread: Option<ActiveThread>,
    fully_addressed_logged: bool,
}
#[derive(Clone, Debug)]
struct GitConfig {
    provider: GitProvider,
    base_url: String,
    token: String,
    project: String,
    project_api_id: Option<String>,
    pr_iid: String,
    verification_command: Option<String>,
    verification_timeout: Duration,
    responder_mode: ResponderMode,
    clear_cmd: Option<String>,
    drip: Option<DripEmitter>,
    allowed_users: Option<BTreeSet<String>>,
    lease: Option<LeaseConfig>,
    integration_mode: IntegrationMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponderMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Debug)]
struct LeaseConfig {
    root: PathBuf,
    ttl: Duration,
    heartbeat: Duration,
    worker_id: Uuid,
}

#[derive(Clone, Debug)]
struct ParallelGitConfig {
    target_branch: String,
    responder_branch: String,
    push_retries: u32,
}

#[derive(Clone, Debug)]
enum IntegrationMode {
    DirectPush,
    Parallel(ParallelGitConfig),
}

#[derive(Clone, Debug)]
struct ActiveThread {
    discussion_id: String,
    comment_id: u64,
    reaction_comment_id: u64,
    summary: String,
    last_seen_note_id: u64,
    prompt_prefix: String,
    prompt_cursor: u64,
    pending_reply: Option<String>,
    non_drip_state: NonDripState,
    lease: Option<ThreadLease>,
}

#[derive(Clone, Debug)]
struct ClaimedThread {
    thread: UnresolvedThread,
    lease: Option<ThreadLease>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NonDripState {
    AwaitingModelOutput,
    PendingIntegration { commit_sha: String },
}

#[derive(Clone, Copy, Debug, Default)]
struct ReplyArtifact;

#[derive(Clone, Debug)]
struct ThreadLease {
    thread_dir: PathBuf,
    lease_path: PathBuf,
    worker_id: Uuid,
    attempt_id: LeaseAttemptId,
    fencing_token: u64,
    acquired_at: u64,
    expires_at: u64,
    heartbeat_at: u64,
}

impl ThreadLease {
    fn trim_wrapping_reply_artifacts(raw: &str) -> String {
        let mut text = raw.trim().to_string();
        if text.starts_with("\",") {
            text = text.trim_start_matches("\",").trim_start().to_string();
        }
        if text.ends_with("\",") {
            text = text.trim_end_matches("\",").trim_end().to_string();
        }
        if text.starts_with('"') && text.ends_with('"') && text.len() > 1 {
            return text[1..(text.len() - 1)].trim().to_string();
        }
        if text.starts_with('"') {
            text = text[1..].trim_start().to_string();
        }
        if text.ends_with('"') {
            let _ = text.pop();
            text = text.trim_end().to_string();
        }
        text
    }

    fn lease_paths(config: &GitConfig, discussion_id: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let lease = config.lease.as_ref()?;
        let lease_root = if lease.root.as_os_str().is_empty() {
            Path::new(DEFAULT_LEASE_ROOT)
        } else {
            lease.root.as_path()
        };
        let project_key = sanitize_lease_component(config.project.as_str());
        let pr_key = sanitize_lease_component(config.pr_iid.as_str());
        let thread_key = sanitize_lease_component(discussion_id);
        let mr_dir = lease_root
            .join(format!("project-{project_key}"))
            .join(format!("mr-{pr_key}"));
        let thread_dir = mr_dir.join(format!("thread-{thread_key}"));
        let lease_path = thread_dir.join("lease.json");
        Some((mr_dir, thread_dir, lease_path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LeaseAttemptId(Uuid);

impl ActiveThread {
    fn monotonic_seconds() -> u64 {
        bindings::wasi::clocks::monotonic_clock::now() / 1_000_000_000
    }

    async fn claim_lease(
        config: &GitConfig,
        discussion_id: &str,
    ) -> Result<Option<ThreadLease>, String> {
        let Some(lease_config) = config.lease.as_ref() else {
            return Ok(None);
        };
        let Some((mr_dir, thread_dir, lease_path)) =
            ThreadLease::lease_paths(config, discussion_id)
        else {
            return Ok(None);
        };
        ensure_directory(mr_dir.as_path()).await?;

        let acquired = matches!(
            mitb_sdk::fs::create_dir(path_to_str(thread_dir.as_path())?).await?,
            mitb_sdk::fs::CreateDirOutcome::Created
        );
        let now = Self::monotonic_seconds();

        let existing = if acquired {
            None
        } else {
            let existing =
                read_thread_lease_file(config, thread_dir.as_path(), lease_path.as_path()).await?;
            let stale = match existing.as_ref() {
                Some(lease) => lease.expires_at <= now,
                None => {
                    // A just-created directory can briefly exist before its lease file lands.
                    // Treat missing lease files as busy unless the directory itself is stale.
                    thread_directory_without_lease_is_stale(
                        thread_dir.as_path(),
                        lease_config.ttl.as_secs().max(1),
                    )
                    .await?
                }
            };
            if !stale {
                return Ok(None);
            }
            ensure_directory(thread_dir.as_path()).await?;
            existing
        };

        let fencing_token = existing
            .as_ref()
            .map(|lease| lease.fencing_token.saturating_add(1))
            .unwrap_or(1);
        let acquired_at = Self::monotonic_seconds();
        let attempt_id = LeaseAttemptId::from_seed(
            format!(
                "{}:{}:{}:{}:{}",
                lease_config.worker_id,
                discussion_id,
                fencing_token,
                acquired_at,
                bindings::wasi::clocks::monotonic_clock::now()
            )
            .as_str(),
        );
        let expires_at = acquired_at.saturating_add(lease_config.ttl.as_secs().max(1));
        let lease = ThreadLease {
            thread_dir: thread_dir.to_path_buf(),
            lease_path: lease_path.to_path_buf(),
            worker_id: lease_config.worker_id.clone(),
            attempt_id,
            fencing_token,
            acquired_at,
            expires_at,
            heartbeat_at: acquired_at,
        };
        write_thread_lease_file(&lease).await?;
        let Some(recorded_lease) =
            read_thread_lease_file(config, thread_dir.as_path(), lease_path.as_path()).await?
        else {
            return Ok(None);
        };
        if !lease_matches_attempt(&recorded_lease, &lease) {
            log::debug!(
                "Lost lease claim race for discussion {}; another worker updated the lease first.",
                discussion_id
            );
            return Ok(None);
        }
        Ok(Some(lease))
    }

    async fn finalize(&self, config: &GitConfig, commit_sha: Option<&str>) -> Result<bool, String> {
        finalize_thread_guard(config, self, commit_sha).await
    }

    async fn release(&self, config: &GitConfig) {
        release_thread_lease(config, self).await;
    }

    async fn refresh_lease(&mut self, config: &GitConfig) -> Result<bool, String> {
        let Some(lease_config) = config.lease.as_ref() else {
            return Ok(true);
        };
        let Some(lease) = self.lease.as_mut() else {
            return Ok(true);
        };

        let Some(current) = read_thread_lease_file(
            config,
            lease.thread_dir.as_path(),
            lease.lease_path.as_path(),
        )
        .await?
        else {
            return Ok(false);
        };
        if !lease_matches_attempt(&current, lease) {
            return Ok(false);
        }

        let now = Self::monotonic_seconds();
        if now.saturating_add(lease_config.heartbeat.as_secs()) < lease.expires_at {
            return Ok(true);
        }

        lease.heartbeat_at = now;
        lease.expires_at = now.saturating_add(lease_config.ttl.as_secs().max(1));
        write_thread_lease_file(lease).await?;
        Ok(true)
    }

    async fn post_reply(&self, config: &GitConfig, reply_body: &str) -> Result<(), String> {
        post_discussion_reply(config, self.discussion_id.as_str(), reply_body).await
    }

    async fn resolve(&self, config: &GitConfig) -> Result<(), String> {
        resolve_discussion(config, self.discussion_id.as_str(), self.comment_id).await
    }

    async fn mark_complete(&self, config: &GitConfig) {
        mark_thread_complete(
            config,
            self.discussion_id.as_str(),
            self.reaction_comment_id,
        )
        .await;
    }

    async fn maybe_capture_reply(&mut self) -> Result<Option<String>, String> {
        if self.pending_reply.is_some() {
            return Ok(self.pending_reply.clone());
        }

        if let Some(reply) = reply_artifact().read().await? {
            self.pending_reply = Some(reply.clone());
            return Ok(Some(reply));
        }

        let (_, delta) =
            terminal_read_since_text(self.prompt_cursor, mitb_sdk::DEFAULT_TERMINAL_MAX_BYTES)
                .await?;
        let scoped_output = scope_terminal_output_to_prompt(delta.as_str(), &self.prompt_prefix);
        let reply = extract_reply(scoped_output)?;
        if let Some(reply) = reply.as_ref() {
            self.pending_reply = Some(reply.clone());
        }
        Ok(reply)
    }

    async fn run_verification_command(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        log::info!(
            "Running verification command for discussion {} (comment {}) with timeout {}s: {}",
            self.discussion_id,
            self.comment_id,
            timeout.as_secs(),
            command
        );
        let child = bindings::mitb::host::process::spawn(
            "bash".to_string(),
            vec!["-lc".to_string(), command.to_string()],
        )
        .await
        .map_err(|error| format!("failed spawning verification command: {error}"))?;
        let (mut stdout, stdout_done) = child.read_stdout().await;
        let (mut stderr, stderr_done) = child.read_stderr().await;

        let stdout_fut = async {
            let mut bytes = Vec::<u8>::new();
            let mut chunk = Vec::<u8>::new();
            while let Some(byte) = stdout.next().await {
                bytes.push(byte);
                chunk.push(byte);
                if byte == b'\n' || chunk.len() >= 1024 {
                    log_verification_chunk("stdout", command, chunk.as_slice()).await;
                    chunk.clear();
                }
            }
            if !chunk.is_empty() {
                log_verification_chunk("stdout", command, chunk.as_slice()).await;
            }
            stdout_done.into_future().await?;
            Ok::<Vec<u8>, String>(bytes)
        };

        let stderr_fut = async {
            let mut bytes = Vec::<u8>::new();
            let mut chunk = Vec::<u8>::new();
            while let Some(byte) = stderr.next().await {
                bytes.push(byte);
                chunk.push(byte);
                if byte == b'\n' || chunk.len() >= 1024 {
                    log_verification_chunk("stderr", command, chunk.as_slice()).await;
                    chunk.clear();
                }
            }
            if !chunk.is_empty() {
                log_verification_chunk("stderr", command, chunk.as_slice()).await;
            }
            stderr_done.into_future().await?;
            Ok::<Vec<u8>, String>(bytes)
        };

        let wait_fut = async {
            match child
                .wait_timeout(mitb_sdk::duration_to_nanos_u64(timeout))
                .await?
            {
                Some(status) => {
                    Ok::<Option<bindings::mitb::host::types::ExitStatus>, String>(Some(status))
                }
                None => {
                    let _ = child.kill().await;
                    Ok(None)
                }
            }
        };

        let streams_fut = async { mitb_sdk::futures::join!(stdout_fut, stderr_fut) };
        mitb_sdk::futures::pin_mut!(wait_fut);
        mitb_sdk::futures::pin_mut!(streams_fut);

        let (wait_result, stdout_bytes, stderr_bytes, stream_drain_timed_out) =
            match mitb_sdk::futures::future::select(wait_fut, streams_fut).await {
                mitb_sdk::futures::future::Either::Left((wait_result, streams_fut)) => {
                    let wait_result = wait_result?;
                    let drain_timeout = async {
                        bindings::wasi::clocks::monotonic_clock::wait_for(
                            VERIFICATION_STREAM_DRAIN_GRACE_NS,
                        )
                        .await;
                    };
                    match mitb_sdk::with_timeout(streams_fut, drain_timeout).await {
                        mitb_sdk::TimeoutOutcome::Completed((stdout_result, stderr_result)) => {
                            (wait_result, stdout_result?, stderr_result?, false)
                        }
                        mitb_sdk::TimeoutOutcome::TimedOut => {
                            log::warn!(
                                "Verification stream drain exceeded {}ms after process exit; continuing.",
                                VERIFICATION_STREAM_DRAIN_GRACE_NS / 1_000_000
                            );
                            (wait_result, Vec::new(), Vec::new(), true)
                        }
                    }
                }
                mitb_sdk::futures::future::Either::Right((
                    (stdout_result, stderr_result),
                    wait_fut,
                )) => (wait_fut.await?, stdout_result?, stderr_result?, false),
            };
        let combined_output = format!(
            "{}{}{}",
            if stream_drain_timed_out {
                "[verification-output] stream drain timed out; see prior live logs for remaining output\n"
            } else {
                ""
            },
            String::from_utf8_lossy(stdout_bytes.as_slice()),
            String::from_utf8_lossy(stderr_bytes.as_slice())
        );

        match wait_result {
            Some(status) if status.success => {
                log::info!("Verification command passed.");
                Ok(())
            }
            Some(status) => {
                let status_text = status
                    .code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| String::from("terminated by signal"));
                log::info!(
                    "Verification command failed with exit status {}: {}",
                    status_text,
                    command
                );
                Err(format!(
                    "verification command exited with status {status_text}: {command}\nOutput:\n{}",
                    mitb_sdk::truncate(combined_output.as_str(), 4000)
                ))
            }
            None => {
                log::info!(
                    "Verification command timed out after {} seconds: {}",
                    timeout.as_secs(),
                    command
                );
                Err(format!(
                    "verification command timed out after {} seconds: {command}\nPartial output:\n{}",
                    timeout.as_secs(),
                    mitb_sdk::truncate(combined_output.as_str(), 4000)
                ))
            }
        }
    }

    async fn rebase_has_conflicts(&self) -> Result<bool, String> {
        let output = run_process(
            "bash",
            vec![
                "-lc".to_string(),
                "if [ -n \"$(git diff --name-only --diff-filter=U)\" ]; then echo yes; else echo no; fi"
                    .to_string(),
            ],
        )
        .await?;
        Ok(String::from_utf8_lossy(output.as_slice()).trim() == "yes")
    }

    async fn integrate_pending_commit(
        &self,
        config: &GitConfig,
        local_commit_sha: &str,
    ) -> Result<IntegrationOutcome, String> {
        let mut latest_commit_sha = local_commit_sha.to_string();
        let parallel = match &config.integration_mode {
            IntegrationMode::DirectPush => {
                return match push_current_branch().await {
                    Ok(()) => Ok(IntegrationOutcome::Integrated {
                        landed_commit_sha: latest_commit_sha,
                    }),
                    Err(error) => Ok(IntegrationOutcome::NeedsPrompt(push_failure_prompt(
                        self.comment_id,
                        latest_commit_sha.as_str(),
                        error.as_str(),
                    ))),
                };
            }
            IntegrationMode::Parallel(parallel) => parallel,
        };

        checkout_branch(parallel.responder_branch.as_str()).await?;
        if rebase_in_progress().await? {
            if self.rebase_has_conflicts().await? {
                return Ok(IntegrationOutcome::NeedsPrompt(
                    integration_conflict_prompt(
                        self,
                        parallel,
                        "rebase is waiting on conflict resolution",
                    )
                    .await?,
                ));
            }
            if let Err(error) =
                run_process("git", vec!["rebase".to_string(), "--continue".to_string()]).await
            {
                if rebase_in_progress().await? || is_rebase_conflict_error(error.as_str()) {
                    return Ok(IntegrationOutcome::NeedsPrompt(
                        integration_conflict_prompt(self, parallel, error.as_str()).await?,
                    ));
                }
                return Err(format!(
                    "failed continuing in-progress rebase before integration: {}",
                    error
                ));
            }
        }
        if repository_has_changes().await? {
            latest_commit_sha = create_commit_for_comment(self.comment_id).await?;
            log::info!(
                "Created follow-up integration commit {} while resolving discussion {}.",
                latest_commit_sha,
                self.discussion_id
            );
        }

        let mut retries_remaining = parallel.push_retries.max(1);
        loop {
            fetch_remote_branch(parallel.target_branch.as_str()).await?;
            if let Err(error) = rebase_onto_remote_branch(parallel.target_branch.as_str()).await {
                if rebase_in_progress().await? || is_rebase_conflict_error(error.as_str()) {
                    return Ok(IntegrationOutcome::NeedsPrompt(
                        integration_conflict_prompt(self, parallel, error.as_str()).await?,
                    ));
                }
                return Err(format!(
                    "failed rebasing responder branch `{}` onto `origin/{}`: {}",
                    parallel.responder_branch, parallel.target_branch, error
                ));
            }

            if let Some(command) = config.verification_command.as_deref()
                && let Err(error) = self
                    .run_verification_command(command, config.verification_timeout)
                    .await
            {
                return Ok(IntegrationOutcome::NeedsPrompt(
                    verification_failure_prompt(
                        self.comment_id,
                        command,
                        error.as_str(),
                        None,
                        None,
                    ),
                ));
            }

            match push_head_to_remote_branch(parallel.target_branch.as_str()).await {
                Ok(()) => {
                    let landed_commit_sha = read_trimmed_stdout(
                        "git",
                        vec!["rev-parse".to_string(), "HEAD".to_string()],
                    )
                    .await?;
                    return Ok(IntegrationOutcome::Integrated { landed_commit_sha });
                }
                Err(error) => {
                    if is_non_fast_forward_push_error(error.as_str()) && retries_remaining > 1 {
                        retries_remaining = retries_remaining.saturating_sub(1);
                        log::info!(
                            "Push to origin/{} lost a race for discussion {}; retrying integration ({} retries remaining).",
                            parallel.target_branch,
                            self.discussion_id,
                            retries_remaining
                        );
                        continue;
                    }

                    if is_non_fast_forward_push_error(error.as_str()) {
                        return Ok(IntegrationOutcome::NeedsPrompt(
                            integration_push_race_prompt(
                                self.comment_id,
                                parallel.target_branch.as_str(),
                                error.as_str(),
                            ),
                        ));
                    }

                    let commit_sha = read_trimmed_stdout(
                        "git",
                        vec!["rev-parse".to_string(), "HEAD".to_string()],
                    )
                    .await
                    .unwrap_or_else(|_| latest_commit_sha.clone());
                    return Ok(IntegrationOutcome::NeedsPrompt(push_failure_prompt(
                        self.comment_id,
                        commit_sha.as_str(),
                        error.as_str(),
                    )));
                }
            }
        }
    }
}

impl ReplyArtifact {
    fn parse_reply_candidate(raw: &str) -> Result<Option<String>, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        if trimmed.contains("<reply") {
            return extract_reply(trimmed);
        }

        let cleaned = sanitize_reply_text(trimmed);
        if cleaned.is_empty() {
            Ok(None)
        } else {
            Ok(Some(cleaned))
        }
    }

    async fn clear(self) {
        let _ = run_process(
            "bash",
            vec!["-lc".to_string(), format!(": > \"{REPLY_ARTIFACT_PATH}\"")],
        )
        .await;
    }

    async fn read(self) -> Result<Option<String>, String> {
        let text = match mitb_sdk::fs::read_text(REPLY_ARTIFACT_PATH).await {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        Self::parse_reply_candidate(text.as_str())
    }
}

fn reply_artifact() -> ReplyArtifact {
    ReplyArtifact
}

impl LeaseAttemptId {
    fn from_seed(seed: &str) -> Self {
        Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
    }

    fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Uuid::parse_str(trimmed).ok().map(Self)
    }
}

impl core::fmt::Display for LeaseAttemptId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug)]
struct UnresolvedThread {
    discussion_id: String,
    comment_id: u64,
    reaction_comment_id: u64,
    summary: String,
    last_seen_note_id: u64,
}

#[derive(Clone, Debug)]
struct ParsedPrUrl {
    provider: GitProvider,
    base_url: String,
    project_path: String,
    pr_iid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GitProvider {
    Gitlab,
    Github { owner: String, repo: String },
}

impl GitProvider {
    fn name(&self) -> &'static str {
        match self {
            GitProvider::Gitlab => "GitLab",
            GitProvider::Github { .. } => "GitHub",
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedNote {
    id: u64,
    author: String,
    body: String,
    resolvable: bool,
    resolved: bool,
    position: Option<NotePosition>,
}

#[derive(Clone, Debug)]
struct NotePosition {
    path: Option<String>,
    new_line: Option<u64>,
    old_line: Option<u64>,
    start_line: Option<u64>,
    end_line: Option<u64>,
    line_code: Option<String>,
    position_type: Option<String>,
}

#[derive(Clone, Debug)]
struct ParsedDiscussion {
    discussion_id: String,
    comment_id: u64,
    notes: Vec<ParsedNote>,
}

#[derive(Clone, Debug)]
struct ParsedGithubReviewComment {
    note: ParsedNote,
    in_reply_to_id: Option<u64>,
}

impl GitConfig {
    async fn from_environment() -> Result<Self, String> {
        let mut config = Self::parse_from_environment()?;
        config.resolve_gitlab_project_id().await?;
        config.initialize_parallel_responder_branch().await?;
        Ok(config)
    }

    fn parse_from_environment() -> Result<Self, String> {
        let token = required_env_any(&[ENV_GIT_TOKEN])?;
        let pr_url = required_env_any(&[ENV_GIT_PR_URL])?;
        let ParsedPrUrl {
            provider,
            base_url,
            project_path,
            pr_iid,
        } = parse_pr_url(pr_url.as_str())?;

        let verification_command =
            optional_env_any(&[ENV_VERIFICATION_COMMAND]).filter(|value| !value.trim().is_empty());
        let clear_cmd =
            optional_env_any(&[ENV_CLEAR_COMMAND]).filter(|value| !value.trim().is_empty());
        let verification_timeout = env_duration_seconds(
            ENV_VERIFICATION_TIMEOUT_SECONDS,
            DEFAULT_VERIFICATION_TIMEOUT,
        );
        let responder_mode = ResponderMode::from_environment();

        let drip = drip_emitter::parse_from_environment();
        let lease = LeaseConfig::parse_from_environment(drip.is_some());

        Ok(Self {
            provider,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            project: project_path,
            project_api_id: None,
            pr_iid,
            verification_command,
            verification_timeout,
            responder_mode,
            clear_cmd,
            allowed_users: parse_allowed_users(),
            drip,
            lease,
            integration_mode: IntegrationMode::DirectPush,
        })
    }

    async fn initialize_parallel_responder_branch(&mut self) -> Result<(), String> {
        let Some(lease) = self.lease.as_ref() else {
            return Ok(());
        };
        if self.drip.is_some() {
            return Ok(());
        }

        let target_branch = current_branch_name().await?;
        if target_branch == "HEAD" {
            return Err(String::from(
                "parallel responder integration requires a checked-out branch (detached HEAD is unsupported)",
            ));
        }

        let worker_id = lease.worker_id.to_string();
        let responder_branch = format!(
            "mitb/parallel/mr-{}/{}",
            sanitize_lease_component(self.pr_iid.as_str()),
            sanitize_lease_component(worker_id.as_str())
        );
        checkout_or_create_branch(responder_branch.as_str(), target_branch.as_str()).await?;
        self.integration_mode = IntegrationMode::Parallel(ParallelGitConfig {
            target_branch,
            responder_branch,
            push_retries: DEFAULT_INTEGRATION_PUSH_RETRIES.max(1),
        });
        Ok(())
    }

    async fn resolve_gitlab_project_id(&mut self) -> Result<(), String> {
        if !matches!(self.provider, GitProvider::Gitlab) {
            return Ok(());
        }
        let project_path = normalize_project_path(self.project.as_str());
        if project_path.is_empty() {
            return Ok(());
        }
        if project_path.chars().all(|ch| ch.is_ascii_digit()) {
            self.project_api_id = Some(project_path);
            return Ok(());
        }

        let lookup_url = format!(
            "{}/api/v4/projects/{}",
            self.base_url,
            url_encode_component(project_path.as_str())
        );
        match git_request_json(
            mitb_sdk::http::HttpMethod::Get,
            lookup_url.as_str(),
            self,
            None,
        )
        .await
        {
            Ok(payload) => {
                if let Some(project_id) = payload.get("id").and_then(Value::as_u64) {
                    self.project_api_id = Some(project_id.to_string());
                } else {
                    log::warn!(
                        "GitLab project lookup for `{}` did not include an `id`; continuing with configured project path.",
                        self.project
                    );
                }
                Ok(())
            }
            Err(error) => {
                if !is_gitlab_project_not_found_error(error.as_str()) {
                    log::warn!(
                        "GitLab project lookup failed for `{}`: {}. Continuing with configured project path.",
                        self.project,
                        error
                    );
                    return Ok(());
                }

                if let Some((project_id, matched_path)) = self
                    .find_gitlab_project_id_from_search(project_path.as_str())
                    .await?
                {
                    self.project_api_id = Some(project_id.to_string());
                    log::info!(
                        "Resolved GitLab project `{}` to id {} using search match `{}` after 404 lookup.",
                        self.project,
                        project_id,
                        matched_path
                    );
                    return Ok(());
                }

                log::warn!(
                    "GitLab project lookup returned 404 for `{}` and search fallback found no match; continuing with configured project path.",
                    self.project
                );
                Ok(())
            }
        }
    }

    async fn find_gitlab_project_id_from_search(
        &self,
        target_project_path: &str,
    ) -> Result<Option<(u64, String)>, String> {
        let search_term = target_project_path
            .rsplit('/')
            .next()
            .unwrap_or(target_project_path)
            .trim();
        if search_term.is_empty() {
            return Ok(None);
        }

        let mut page = 1_usize;
        loop {
            let url = format!(
                "{}/api/v4/projects?search={}&simple=true&per_page={}&page={}",
                self.base_url,
                url_encode_component(search_term),
                DEFAULT_PAGE_SIZE,
                page
            );
            let payload =
                git_request_json(mitb_sdk::http::HttpMethod::Get, url.as_str(), self, None).await?;
            let Some(projects) = payload.as_array() else {
                return Err(format!(
                    "GitLab projects search response was not an array: {}",
                    mitb_sdk::truncate(payload.to_string().as_str(), 1024)
                ));
            };

            let mut fallback_suffix_match = None::<(u64, String)>;
            for project in projects {
                let Some(project_id) = project.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let candidate_path = normalize_project_path(
                    project
                        .get("path_with_namespace")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                if candidate_path.is_empty() {
                    continue;
                }
                if candidate_path.eq_ignore_ascii_case(target_project_path) {
                    return Ok(Some((project_id, candidate_path)));
                }
                if fallback_suffix_match.is_none()
                    && project_path_is_suffix(target_project_path, candidate_path.as_str())
                {
                    fallback_suffix_match = Some((project_id, candidate_path));
                }
            }

            if fallback_suffix_match.is_some() {
                return Ok(fallback_suffix_match);
            }
            if projects.len() < DEFAULT_PAGE_SIZE {
                break;
            }
            page = page.saturating_add(1);
        }
        Ok(None)
    }

    fn gitlab_project_path_segment(&self) -> &str {
        self.project_api_id
            .as_deref()
            .unwrap_or(self.project.as_str())
    }

    fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    fn parse_thread_lease_record(
        &self,
        value: &Value,
        thread_dir: &Path,
        lease_path: &Path,
    ) -> Option<ThreadLease> {
        let worker_id = Uuid::parse_str(value.get("worker_id")?.as_str()?.trim()).ok()?;
        let attempt_id = LeaseAttemptId::parse(value.get("attempt_id")?.as_str()?)?;
        let fencing_token = value.get("fencing_token")?.as_u64()?;
        let acquired_at = value.get("acquired_at")?.as_u64()?;
        let expires_at = value.get("expires_at")?.as_u64()?;
        let heartbeat_at = value
            .get("heartbeat_at")
            .and_then(Value::as_u64)
            .unwrap_or(acquired_at);
        Some(ThreadLease {
            thread_dir: thread_dir.to_path_buf(),
            lease_path: lease_path.to_path_buf(),
            worker_id,
            attempt_id,
            fencing_token,
            acquired_at,
            expires_at,
            heartbeat_at,
        })
    }

    fn apply_auth_header(
        &self,
        request: mitb_sdk::http::HttpRequest,
    ) -> mitb_sdk::http::HttpRequest {
        match &self.provider {
            GitProvider::Gitlab => request
                .header("private-token", self.token.as_bytes().to_vec())
                .header(
                    "authorization",
                    format!("Bearer {}", self.token).into_bytes(),
                )
                .header("job-token", self.token.as_bytes().to_vec()),
            GitProvider::Github { .. } => request
                .header(
                    "authorization",
                    format!("Bearer {}", self.token).into_bytes(),
                )
                .header("accept", b"application/vnd.github+json".to_vec()),
        }
    }

    async fn list_unresolved_threads(&self) -> Result<Vec<UnresolvedThread>, String> {
        match &self.provider {
            GitProvider::Gitlab => providers::gitlab::list_unresolved_threads(self).await,
            GitProvider::Github { .. } => providers::github::list_unresolved_threads(self).await,
        }
    }

    async fn send_reply_request(
        &self,
        discussion_id: &str,
        payload: Vec<u8>,
    ) -> Result<mitb_sdk::http::HttpResponse, String> {
        match &self.provider {
            GitProvider::Gitlab => {
                let url = providers::gitlab::discussion_reply_url(self, discussion_id);
                git_send_request(
                    mitb_sdk::http::HttpMethod::Post,
                    url.as_str(),
                    self,
                    Some(payload),
                )
                .await
            }
            GitProvider::Github { .. } => {
                let comment_id = discussion_id
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("invalid GitHub comment id `{discussion_id}`"))?;
                let url = providers::github::review_comment_reply_url(self, comment_id)?;
                git_send_request(
                    mitb_sdk::http::HttpMethod::Post,
                    url.as_str(),
                    self,
                    Some(payload),
                )
                .await
            }
        }
    }

    async fn send_reaction_request(
        &self,
        _discussion_id: &str,
        comment_id: u64,
        reaction: &str,
    ) -> Result<mitb_sdk::http::HttpResponse, String> {
        match &self.provider {
            GitProvider::Gitlab => {
                let url =
                    providers::gitlab::merge_request_note_reaction_url(self, comment_id, reaction);
                git_send_request(mitb_sdk::http::HttpMethod::Post, url.as_str(), self, None).await
            }
            GitProvider::Github { .. } => {
                let payload =
                    serde_json::to_vec(&json!({ "content": reaction })).map_err(|error| {
                        format!("failed serializing github reaction payload: {error}")
                    })?;
                let url = providers::github::review_comment_reaction_url(self, comment_id)?;
                git_send_request(
                    mitb_sdk::http::HttpMethod::Post,
                    url.as_str(),
                    self,
                    Some(payload),
                )
                .await
            }
        }
    }

    async fn send_resolve_request(
        &self,
        discussion_id: &str,
        _comment_id: u64,
    ) -> Result<mitb_sdk::http::HttpResponse, String> {
        match &self.provider {
            GitProvider::Gitlab => {
                let payload =
                    serde_json::to_vec(&json!({ "resolved": true })).map_err(|error| {
                        format!("failed serializing gitlab discussion resolution payload: {error}")
                    })?;
                let url = providers::gitlab::discussion_resolve_url(self, discussion_id);
                git_send_request(
                    mitb_sdk::http::HttpMethod::Put,
                    url.as_str(),
                    self,
                    Some(payload),
                )
                .await
            }
            GitProvider::Github { .. } => Err(String::from(
                "GitHub does not support explicit thread resolution via REST in this policy",
            )),
        }
    }

    async fn fetch_new_thread_replies(
        &self,
        discussion_id: &str,
        since_note_id: u64,
    ) -> Result<(Option<String>, u64), String> {
        match &self.provider {
            GitProvider::Gitlab => {
                providers::gitlab::fetch_new_thread_replies(self, discussion_id, since_note_id)
                    .await
            }
            GitProvider::Github { .. } => {
                providers::github::fetch_new_thread_replies(self, discussion_id, since_note_id)
                    .await
            }
        }
    }
}

impl Policy for GitResponder {
    async fn act(&mut self, contents: String) -> ActionResult {
        let config = self.ensure_config().await?.clone();

        // If the drip emitter has a running pipeline, delegate to it.
        if let Some(drip) = self.config.as_mut().and_then(|c| c.drip.as_mut()) {
            if let Some(active) = self.active_thread.take() {
                return self.finalize_active_thread(&config, active, contents).await;
            }
            return drip.run(&config).await;
        }

        if let Some(active) = self.active_thread.take() {
            return self.finalize_active_thread(&config, active, contents).await;
        }

        self.select_next_thread(&config).await
    }
}

impl GitResponder {
    async fn ensure_config(&mut self) -> Result<&GitConfig, String> {
        if self.config.is_none() {
            let config = GitConfig::from_environment().await?;
            log::info!(
                "Initialized git-responder for {:?} project `{}` PR `{}` (read-only mode: {}, filesystem leases: {}, parallel branch: {}).",
                config.provider,
                config.project,
                config.pr_iid,
                config.responder_mode.is_read_only(),
                config.lease.is_some(),
                config.integration_mode.is_parallel()
            );
            self.config = Some(config);
        }
        self.config
            .as_ref()
            .ok_or_else(|| String::from("missing Git configuration"))
    }

    async fn select_next_thread(&mut self, config: &GitConfig) -> ActionResult {
        let unresolved_threads = list_unresolved_threads(config).await?;
        for unresolved in unresolved_threads.iter() {
            let _ = self
                .addressed_discussions
                .remove(unresolved.discussion_id.as_str());
        }

        let reward = percent_addressed(self.addressed_discussions.len(), unresolved_threads.len());
        report_reward!(reward);

        if unresolved_threads.is_empty() {
            if !self.fully_addressed_logged {
                log::info!(
                    "All tracked unresolved git threads are addressed (reward={reward:.4})."
                );
                self.fully_addressed_logged = true;
            }
            return Ok(Action::Wait);
        }
        self.fully_addressed_logged = false;

        let mut selected = None::<ClaimedThread>;
        for candidate in unresolved_threads {
            let lease = ActiveThread::claim_lease(config, candidate.discussion_id.as_str()).await?;
            if config.lease.is_some() && lease.is_none() {
                continue;
            }
            selected = Some(ClaimedThread {
                thread: candidate,
                lease,
            });
            break;
        }

        let Some(ClaimedThread { thread, lease }) = selected else {
            log::debug!("All unresolved git threads are currently leased by other responders.");
            return Ok(Action::Wait);
        };
        let prompt_cursor = terminal_head().await;
        let prompt = format!(
            "mitb-thread-{}-{}-{}",
            thread.discussion_id,
            thread.comment_id,
            bindings::wasi::clocks::monotonic_clock::now()
        );
        self.active_thread = Some(ActiveThread {
            discussion_id: thread.discussion_id.clone(),
            comment_id: thread.comment_id,
            reaction_comment_id: thread.reaction_comment_id,
            summary: thread.summary.clone(),
            last_seen_note_id: thread.last_seen_note_id,
            prompt_prefix: prompt.clone(),
            prompt_cursor,
            pending_reply: None,
            non_drip_state: NonDripState::AwaitingModelOutput,
            lease,
        });
        mark_thread_in_progress(
            config,
            thread.discussion_id.as_str(),
            thread.reaction_comment_id,
        )
        .await;
        ensure_responder_branch_before_thread(config).await?;

        reply_artifact().clear().await;
        let prompt = build_thread_prompt(
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
            prompt!(prompt)
        }
    }

    async fn finalize_active_thread(
        &mut self,
        config: &GitConfig,
        mut active: ActiveThread,
        _contents: String,
    ) -> ActionResult {
        if !active.refresh_lease(config).await? {
            log::info!(
                "Lease ownership changed for discussion {}; aborting finalize for this attempt.",
                active.discussion_id
            );
            reply_artifact().clear().await;
            return Ok(Action::Wait);
        }

        let reply = active.maybe_capture_reply().await?;

        match active.non_drip_state.clone() {
            NonDripState::PendingIntegration { commit_sha } => {
                log::info!(
                    "Retrying integration for discussion {} using commit {}.",
                    active.discussion_id,
                    commit_sha
                );
                let integration = active
                    .integrate_pending_commit(config, commit_sha.as_str())
                    .await?;
                self.handle_integration_outcome(config, active, integration, reply)
                    .await
            }
            NonDripState::AwaitingModelOutput => {
                if repository_has_changes().await? {
                    match config.responder_mode {
                        ResponderMode::ReadWrite => {
                            log::info!(
                                "Detected repository changes for discussion {} (comment {}).",
                                active.discussion_id,
                                active.comment_id
                            );
                            if let Some(command) = config.verification_command.as_deref()
                                && let Err(error) = active
                                    .run_verification_command(command, config.verification_timeout)
                                    .await
                            {
                                let comment_id = active.comment_id;
                                let (new_thread_replies, retrieval_warning) =
                                    match fetch_new_thread_replies(
                                        config,
                                        active.discussion_id.as_str(),
                                        active.last_seen_note_id,
                                    )
                                    .await
                                    {
                                        Ok((new_replies, latest_note_id)) => {
                                            active.last_seen_note_id = latest_note_id;
                                            (new_replies, None)
                                        }
                                        Err(fetch_error) => (None, Some(fetch_error)),
                                    };
                                active.prompt_cursor = terminal_head().await;
                                self.active_thread = Some(active);
                                reply_artifact().clear().await;
                                return prompt!(verification_failure_prompt(
                                    comment_id,
                                    command,
                                    error.as_str(),
                                    new_thread_replies.as_deref(),
                                    retrieval_warning.as_deref(),
                                ));
                            }

                            log::info!(
                                "Creating commit for discussion {} (comment {}).",
                                active.discussion_id,
                                active.comment_id
                            );
                            let commit_sha = create_commit_for_comment(active.comment_id).await?;
                            log::info!(
                                "Created commit {} for discussion {}.",
                                commit_sha,
                                active.discussion_id
                            );
                            active.non_drip_state = NonDripState::PendingIntegration {
                                commit_sha: commit_sha.clone(),
                            };
                            let integration = active
                                .integrate_pending_commit(config, commit_sha.as_str())
                                .await?;
                            return self
                                .handle_integration_outcome(config, active, integration, reply)
                                .await;
                        }
                        ResponderMode::ReadOnly => {
                            log::info!(
                                "Read-only mode enabled; ignoring local code changes for discussion {}.",
                                active.discussion_id
                            );
                        }
                    }
                }

                let Some(reply) = reply else {
                    active.prompt_cursor = terminal_head().await;
                    let reminder = missing_reply_prompt(
                        active.summary.as_str(),
                        active.comment_id,
                        config.responder_mode,
                    );
                    self.active_thread = Some(active);
                    reply_artifact().clear().await;
                    return prompt!(reminder);
                };

                if !active.finalize(config, None).await? {
                    active.release(config).await;
                    reply_artifact().clear().await;
                    return Ok(Action::Wait);
                }

                self.complete_thread(config, active, reply.as_str()).await?;
                Ok(Action::Wait)
            }
        }
    }

    async fn handle_integration_outcome(
        &mut self,
        config: &GitConfig,
        active: ActiveThread,
        integration: IntegrationOutcome,
        reply: Option<String>,
    ) -> ActionResult {
        match integration {
            IntegrationOutcome::Integrated { landed_commit_sha } => {
                self.finalize_thread_with_commit(config, active, landed_commit_sha.as_str(), reply)
                    .await
            }
            IntegrationOutcome::NeedsPrompt(prompt) => {
                self.requeue_active_with_prompt(active, prompt).await
            }
        }
    }

    async fn finalize_thread_with_commit(
        &mut self,
        config: &GitConfig,
        active: ActiveThread,
        landed_commit_sha: &str,
        reply: Option<String>,
    ) -> ActionResult {
        if !active.finalize(config, Some(landed_commit_sha)).await? {
            active.release(config).await;
            reply_artifact().clear().await;
            return Ok(Action::Wait);
        }

        let discussion_id = active.discussion_id.clone();
        let completion_reply = addressed_commit_reply(landed_commit_sha, reply.as_deref());
        self.complete_thread(config, active, completion_reply.as_str())
            .await?;
        log::info!(
            "Resolved discussion {} with integrated commit {}.",
            discussion_id,
            landed_commit_sha
        );
        Ok(Action::Wait)
    }

    async fn complete_thread(
        &mut self,
        config: &GitConfig,
        active: ActiveThread,
        reply_body: &str,
    ) -> Result<(), String> {
        active.post_reply(config, reply_body).await?;
        active.resolve(config).await?;
        active.mark_complete(config).await;
        active.release(config).await;
        let _ = self.addressed_discussions.insert(active.discussion_id);
        restore_target_branch_after_thread(config).await?;
        Ok(())
    }

    async fn requeue_active_with_prompt(
        &mut self,
        mut active: ActiveThread,
        prompt_text: String,
    ) -> ActionResult {
        active.prompt_cursor = terminal_head().await;
        self.active_thread = Some(active);
        reply_artifact().clear().await;
        prompt!(prompt_text)
    }
}

impl ResponderMode {
    fn from_environment() -> Self {
        if env_flag(ENV_READ_ONLY) {
            Self::ReadOnly
        } else {
            Self::ReadWrite
        }
    }

    fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

impl IntegrationMode {
    fn is_parallel(&self) -> bool {
        matches!(self, Self::Parallel(_))
    }
}

fn env_duration_seconds(env_key: &str, default_duration: Duration) -> Duration {
    env::env_duration_seconds(env_key, default_duration)
}

fn env_flag(env_key: &str) -> bool {
    env::env_flag(env_key)
}

fn parse_bool_env_value(raw_value: &str) -> bool {
    env::parse_bool_env_value(raw_value)
}

impl LeaseConfig {
    fn parse_from_environment(drip_enabled: bool) -> Option<Self> {
        if drip_enabled {
            return None;
        }

        if optional_env_any(&[ENV_GIT_PARALLEL_FILESYSTEM_LEASES])
            .is_some_and(|value| !parse_bool_env_value(value.as_str()))
        {
            return None;
        }

        let ttl = env_duration_seconds(ENV_GIT_LEASE_TTL_SECONDS, DEFAULT_LEASE_TTL);
        let ttl_seconds = ttl.as_secs().max(1);
        let heartbeat_default_seconds = (ttl_seconds / 3).max(5);
        let heartbeat_raw = env_duration_seconds(
            ENV_GIT_LEASE_HEARTBEAT_SECONDS,
            Duration::from_secs(heartbeat_default_seconds),
        );
        let heartbeat_seconds = heartbeat_raw.as_secs().max(1).min(ttl_seconds);
        let shared_root = optional_env_any(&[ENV_SHARED_ROOT])
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty());
        let root = optional_env_any(&[ENV_GIT_LEASE_ROOT])
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty())
            .map(|raw| Self::normalize_root_for_guest(raw.as_str(), shared_root.as_deref()))
            .or_else(|| {
                shared_root.as_deref().map(|shared_root| {
                    format!("{}/leases", Self::shared_root_guest_mount(shared_root))
                })
            })
            .unwrap_or_else(|| String::from(DEFAULT_LEASE_ROOT));
        let root = PathBuf::from(root);
        let worker_id = optional_env_any(&[ENV_GIT_WORKER_ID])
            .and_then(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Uuid::parse_str(trimmed).ok()
                }
            })
            .unwrap_or_else(Self::generate_worker_id);

        Some(Self {
            root,
            ttl: Duration::from_secs(ttl_seconds),
            heartbeat: Duration::from_secs(heartbeat_seconds),
            worker_id,
        })
    }

    fn shared_root_guest_mount(shared_root: &str) -> String {
        let shared_root = shared_root
            .trim()
            .trim_start_matches("./")
            .trim_end_matches('/');
        if shared_root.is_empty() || shared_root.starts_with('/') {
            return String::from("mitb-shared");
        }
        shared_root.to_string()
    }

    fn normalize_root_for_guest(raw_root: &str, shared_root: Option<&str>) -> String {
        let root = raw_root.trim().trim_end_matches('/');
        if root.is_empty() {
            return String::from(DEFAULT_LEASE_ROOT);
        }

        if let Some(shared_root) = shared_root {
            let shared_root = shared_root.trim().trim_end_matches('/');
            if !shared_root.is_empty() && shared_root.starts_with('/') {
                if root == shared_root {
                    return String::from("mitb-shared");
                }
                if let Some(suffix) = root
                    .strip_prefix(shared_root)
                    .and_then(|remaining| remaining.strip_prefix('/'))
                {
                    if suffix.is_empty() {
                        return String::from("mitb-shared");
                    }
                    return format!("mitb-shared/{suffix}");
                }
            }
        }

        root.trim_start_matches("./").to_string()
    }

    fn generate_worker_id() -> Uuid {
        let seed = format!("worker:{}", bindings::wasi::clocks::monotonic_clock::now());
        Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes())
    }
}

async fn current_branch_name() -> Result<String, String> {
    read_trimmed_stdout(
        "git",
        vec![
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string(),
        ],
    )
    .await
}

async fn checkout_or_create_branch(branch: &str, start_point: &str) -> Result<(), String> {
    run_process(
        "bash",
        vec![
            "-lc".to_string(),
            "if git show-ref --verify --quiet \"refs/heads/$1\"; then git checkout \"$1\"; else git checkout -b \"$1\" \"$2\"; fi".to_string(),
            "mitb-git-parallel-branch".to_string(),
            branch.to_string(),
            start_point.to_string(),
        ],
    )
    .await
    .map(|_| ())
}

fn parse_allowed_users() -> Option<BTreeSet<String>> {
    env::parse_allowed_users()
}

fn parse_pr_url(raw: &str) -> Result<ParsedPrUrl, String> {
    env::parse_pr_url(raw)
}

fn required_env_any(names: &[&str]) -> Result<String, String> {
    env::required_env_any(names)
}

fn optional_env_any(names: &[&str]) -> Option<String> {
    env::optional_env_any(names)
}

async fn list_unresolved_threads(config: &GitConfig) -> Result<Vec<UnresolvedThread>, String> {
    config.list_unresolved_threads().await
}

fn author_in_allowlist(config: &GitConfig, author: &str) -> bool {
    thread_context::author_in_allowlist(config, author)
}

fn starts_with_mitb_prefix(body: &str) -> bool {
    thread_context::starts_with_mitb_prefix(body)
}

fn parse_github_review_comment(comment: &Value) -> Option<ParsedGithubReviewComment> {
    thread_context::parse_github_review_comment(comment)
}

fn parse_unresolved_discussion(discussion: &Value) -> Option<ParsedDiscussion> {
    thread_context::parse_unresolved_discussion(discussion)
}

fn parse_discussion_notes(notes_value: Option<&Value>) -> Option<Vec<ParsedNote>> {
    thread_context::parse_discussion_notes(notes_value)
}

async fn format_discussion_summary(discussion_id: &str, notes: &[ParsedNote]) -> String {
    thread_context::format_discussion_summary(discussion_id, notes).await
}

async fn format_github_comment_summary(comment_id: u64, note: &ParsedNote) -> String {
    thread_context::format_github_comment_summary(comment_id, note).await
}

fn max_note_id(notes: &[ParsedNote]) -> u64 {
    thread_context::max_note_id(notes)
}

fn describe_position(position: &NotePosition) -> String {
    thread_context::describe_position(position)
}

fn build_thread_prompt(summary: &str, comment_id: u64, responder_mode: ResponderMode) -> String {
    reply::build_thread_prompt(summary, comment_id, responder_mode)
}

fn missing_reply_prompt(summary: &str, comment_id: u64, responder_mode: ResponderMode) -> String {
    reply::missing_reply_prompt(summary, comment_id, responder_mode)
}

fn addressed_commit_reply(commit_sha: &str, reply: Option<&str>) -> String {
    reply::addressed_commit_reply(commit_sha, reply)
}

fn verification_failure_prompt(
    comment_id: u64,
    command: &str,
    error: &str,
    new_thread_replies: Option<&str>,
    retrieval_warning: Option<&str>,
) -> String {
    reply::verification_failure_prompt(
        comment_id,
        command,
        error,
        new_thread_replies,
        retrieval_warning,
    )
}

fn push_failure_prompt(comment_id: u64, commit_sha: &str, error: &str) -> String {
    reply::push_failure_prompt(comment_id, commit_sha, error)
}

fn extract_reply(text: &str) -> Result<Option<String>, String> {
    reply::extract_reply(text)
}

fn sanitize_reply_text(raw: &str) -> String {
    reply::sanitize_reply_text(raw)
}

fn scope_terminal_output_to_prompt<'a>(terminal_output: &'a str, prompt_prefix: &str) -> &'a str {
    reply::scope_terminal_output_to_prompt(terminal_output, prompt_prefix)
}

fn percent_addressed(addressed_count: usize, unresolved_count: usize) -> f64 {
    let total = addressed_count.saturating_add(unresolved_count);
    if total == 0 {
        1.0
    } else {
        (addressed_count as f64) / (total as f64)
    }
}

fn normalize_project_path(raw: &str) -> String {
    raw.split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn project_path_is_suffix(target_path: &str, candidate_path: &str) -> bool {
    let target = normalize_project_path(target_path).to_ascii_lowercase();
    let candidate = normalize_project_path(candidate_path).to_ascii_lowercase();
    if target.is_empty() || candidate.is_empty() {
        return false;
    }
    if target == candidate {
        return true;
    }
    target.ends_with(format!("/{candidate}").as_str())
}

fn is_gitlab_project_not_found_error(error: &str) -> bool {
    let lowered = error.to_ascii_lowercase();
    lowered.contains("gitlab api")
        && lowered.contains("http 404")
        && lowered.contains("project not found")
}

fn url_encode_component(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(format!("%{:02X}", byte).as_str());
        }
    }
    encoded
}

async fn post_discussion_reply(
    config: &GitConfig,
    discussion_id: &str,
    reply_body: &str,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&json!({ "body": with_git_reply_preamble(reply_body) }))
        .map_err(|error| format!("failed serializing discussion reply payload: {error}"))?;
    let response = config.send_reply_request(discussion_id, payload).await?;
    if response.status != 200 && response.status != 201 {
        let body = response
            .text()
            .unwrap_or_else(|_| String::from("<non-utf8 response body>"));
        return Err(format!(
            "{} reply API returned HTTP {}: {}",
            config.provider_name(),
            response.status,
            mitb_sdk::truncate(body.as_str(), 2048),
        ));
    }
    reply_artifact().clear().await;
    Ok(())
}

fn with_git_reply_preamble(reply_body: &str) -> String {
    reply::with_git_reply_preamble(reply_body)
}

async fn resolve_discussion(
    config: &GitConfig,
    discussion_id: &str,
    comment_id: u64,
) -> Result<(), String> {
    if matches!(config.provider, GitProvider::Github { .. }) {
        // GitHub review comments do not have a matching REST resolve endpoint.
        return Ok(());
    }
    let response = config
        .send_resolve_request(discussion_id, comment_id)
        .await?;
    if response.status != 200 && response.status != 201 && response.status != 204 {
        let body = response
            .text()
            .unwrap_or_else(|_| String::from("<non-utf8 response body>"));
        return Err(format!(
            "{} resolve API returned HTTP {}: {}",
            config.provider_name(),
            response.status,
            mitb_sdk::truncate(body.as_str(), 2048),
        ));
    }
    Ok(())
}

async fn fetch_new_thread_replies(
    config: &GitConfig,
    discussion_id: &str,
    since_note_id: u64,
) -> Result<(Option<String>, u64), String> {
    config
        .fetch_new_thread_replies(discussion_id, since_note_id)
        .await
}

fn path_to_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path `{}` is not valid utf-8", path.display()))
}

fn sanitize_lease_component(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut prior_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            output.push(ch);
            prior_dash = false;
        } else if !prior_dash {
            output.push('-');
            prior_dash = true;
        }
    }
    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        String::from("unknown")
    } else {
        trimmed.to_string()
    }
}

async fn ensure_directory(path: &Path) -> Result<(), String> {
    mitb_sdk::fs::create_dir_all(path_to_str(path)?).await
}

async fn thread_directory_without_lease_is_stale(
    thread_dir: &Path,
    stale_after_seconds: u64,
) -> Result<bool, String> {
    let stale_after_seconds = stale_after_seconds.max(1);
    let age_seconds = match mitb_sdk::fs::age_seconds(path_to_str(thread_dir)?).await? {
        Some(age_seconds) => age_seconds,
        None => return Ok(true),
    };
    Ok(age_seconds >= stale_after_seconds)
}

async fn read_thread_lease_text(lease_path: &Path) -> Result<Option<String>, String> {
    let text = match mitb_sdk::fs::read_text_if_exists(path_to_str(lease_path)?).await? {
        Some(text) => text,
        None => return Ok(None),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

async fn read_thread_lease_file(
    config: &GitConfig,
    thread_dir: &Path,
    lease_path: &Path,
) -> Result<Option<ThreadLease>, String> {
    let text = match read_thread_lease_text(lease_path).await? {
        Some(text) => text,
        None => return Ok(None),
    };
    let value = match serde_json::from_str::<Value>(text.as_str()) {
        Ok(value) => value,
        Err(error) => {
            log::warn!(
                "Ignoring malformed lease file `{}` for thread directory `{}`: {}",
                lease_path.display(),
                thread_dir.display(),
                error
            );
            return Ok(None);
        }
    };
    Ok(config.parse_thread_lease_record(&value, thread_dir, lease_path))
}

async fn write_thread_lease_file(lease: &ThreadLease) -> Result<(), String> {
    mitb_sdk::fs::create_dir_all(path_to_str(lease.thread_dir.as_path())?).await?;
    let payload = serde_json::to_string(&json!({
        "worker_id": lease.worker_id.to_string(),
        "attempt_id": lease.attempt_id.to_string(),
        "fencing_token": lease.fencing_token,
        "acquired_at": lease.acquired_at,
        "expires_at": lease.expires_at,
        "heartbeat_at": lease.heartbeat_at,
    }))
    .map_err(|error| format!("failed serializing thread lease payload: {error}"))?;
    mitb_sdk::fs::write_text_atomic(path_to_str(lease.lease_path.as_path())?, payload.as_str())
        .await
}

fn lease_matches_attempt(current: &ThreadLease, expected: &ThreadLease) -> bool {
    current.worker_id == expected.worker_id
        && current.attempt_id == expected.attempt_id
        && current.fencing_token == expected.fencing_token
}

async fn release_thread_lease(config: &GitConfig, active: &ActiveThread) {
    let Some(lease) = active.lease.as_ref() else {
        return;
    };

    let current = match read_thread_lease_file(
        config,
        lease.thread_dir.as_path(),
        lease.lease_path.as_path(),
    )
    .await
    {
        Ok(current) => current,
        Err(error) => {
            log::warn!(
                "Failed reading lease file while releasing discussion {}: {}",
                active.discussion_id,
                error
            );
            return;
        }
    };
    let Some(mut current) = current else {
        return;
    };
    if !lease_matches_attempt(&current, lease) {
        return;
    }

    current.expires_at = 0;
    current.heartbeat_at = ActiveThread::monotonic_seconds();
    if let Err(error) = write_thread_lease_file(&current).await {
        log::warn!(
            "Failed writing release marker for discussion {}: {}",
            active.discussion_id,
            error
        );
    }
}

async fn finalize_thread_guard(
    config: &GitConfig,
    active: &ActiveThread,
    commit_sha: Option<&str>,
) -> Result<bool, String> {
    if let Some(expected_lease) = active.lease.as_ref() {
        let current_lease = read_thread_lease_file(
            config,
            expected_lease.thread_dir.as_path(),
            expected_lease.lease_path.as_path(),
        )
        .await?;
        let Some(current_lease) = current_lease else {
            log::info!(
                "Skipping finalize for discussion {} because lease is missing.",
                active.discussion_id
            );
            return Ok(false);
        };
        if !lease_matches_attempt(&current_lease, expected_lease) {
            log::info!(
                "Skipping finalize for discussion {} because lease ownership changed.",
                active.discussion_id
            );
            return Ok(false);
        }
        if current_lease.expires_at <= ActiveThread::monotonic_seconds() {
            log::info!(
                "Skipping finalize for discussion {} because lease expired.",
                active.discussion_id
            );
            return Ok(false);
        }
    }

    if !thread_is_still_unresolved(config, active.discussion_id.as_str()).await? {
        log::info!(
            "Skipping finalize for discussion {} because it is already resolved.",
            active.discussion_id
        );
        return Ok(false);
    }

    if let Some(commit_sha) = commit_sha
        && !commit_is_on_current_head(commit_sha).await?
    {
        log::info!(
            "Skipping finalize for discussion {} because commit {} is not on HEAD.",
            active.discussion_id,
            commit_sha
        );
        return Ok(false);
    }

    Ok(true)
}

async fn thread_is_still_unresolved(
    config: &GitConfig,
    discussion_id: &str,
) -> Result<bool, String> {
    let unresolved = list_unresolved_threads(config).await?;
    Ok(unresolved
        .iter()
        .any(|thread| thread.discussion_id == discussion_id))
}

async fn commit_is_on_current_head(commit_sha: &str) -> Result<bool, String> {
    let output = run_process(
        "bash",
        vec![
            "-lc".to_string(),
            "if git merge-base --is-ancestor \"$1\" HEAD; then echo yes; else echo no; fi"
                .to_string(),
            "mitb-git-lease-head".to_string(),
            commit_sha.to_string(),
        ],
    )
    .await?;
    Ok(String::from_utf8_lossy(output.as_slice()).trim() == "yes")
}

async fn mark_thread_in_progress(config: &GitConfig, discussion_id: &str, comment_id: u64) {
    for reaction in REACTION_THREAD_PICKED_UP {
        if let Err(error) = add_thread_reaction(config, discussion_id, comment_id, reaction).await {
            log::warn!(
                "Failed applying `{}` reaction on thread {} note {}: {}",
                reaction,
                discussion_id,
                comment_id,
                error
            );
        }
    }
}

async fn mark_thread_complete(config: &GitConfig, discussion_id: &str, comment_id: u64) {
    if let Err(error) =
        add_thread_reaction(config, discussion_id, comment_id, REACTION_THREAD_COMPLETE).await
    {
        log::warn!(
            "Failed applying `{}` reaction on thread {} note {}: {}",
            REACTION_THREAD_COMPLETE,
            discussion_id,
            comment_id,
            error
        );
    }
}

async fn add_thread_reaction(
    config: &GitConfig,
    discussion_id: &str,
    comment_id: u64,
    reaction: &str,
) -> Result<(), String> {
    let Some(mapped_reaction) = map_reaction_for_provider(&config.provider, reaction) else {
        return Ok(());
    };
    let response = config
        .send_reaction_request(discussion_id, comment_id, mapped_reaction.as_str())
        .await?;
    if response.status == 200 || response.status == 201 || response.status == 409 {
        return Ok(());
    }
    let body = response
        .text()
        .unwrap_or_else(|_| String::from("<non-utf8 response body>"));
    if (response.status == 400 || response.status == 422)
        && body.to_ascii_lowercase().contains("already")
    {
        return Ok(());
    }
    Err(format!(
        "{} reaction API returned HTTP {} for `{}` on note {}: {}",
        config.provider_name(),
        response.status,
        mapped_reaction,
        comment_id,
        mitb_sdk::truncate(body.as_str(), 2048),
    ))
}

fn map_reaction_for_provider(provider: &GitProvider, reaction: &str) -> Option<String> {
    let normalized = reaction.trim_matches(':').to_ascii_lowercase();
    match provider {
        GitProvider::Gitlab => Some(normalized),
        GitProvider::Github { .. } => match normalized.as_str() {
            "eye" => Some(String::from("eyes")),
            // Keep the provider-neutral completion signal in core flow and
            // map it to GitHub's preferred reaction here.
            "ballot_box_with_check" => Some(String::from("rocket")),
            _ => None,
        },
    }
}

async fn git_request_json(
    method: mitb_sdk::http::HttpMethod,
    url: &str,
    config: &GitConfig,
    body: Option<Vec<u8>>,
) -> Result<Value, String> {
    let response = git_send_request(method, url, config, body).await?;
    let response_text = response.text().map_err(|error| {
        format!(
            "{} response from {url} was not valid utf-8: {error}",
            config.provider_name()
        )
    })?;
    if response.status != 200 {
        return Err(format!(
            "{} API at {url} returned HTTP {}: {}",
            config.provider_name(),
            response.status,
            mitb_sdk::truncate(response_text.as_str(), 2048),
        ));
    }

    serde_json::from_str(response_text.as_str()).map_err(|error| {
        format!(
            "failed parsing {} JSON from {url}: {error}",
            config.provider_name()
        )
    })
}

async fn git_send_request(
    method: mitb_sdk::http::HttpMethod,
    url: &str,
    config: &GitConfig,
    body: Option<Vec<u8>>,
) -> Result<mitb_sdk::http::HttpResponse, String> {
    let mut request = mitb_sdk::http::HttpRequest::new(method, url.to_string())
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .first_byte_timeout(HTTP_FIRST_BYTE_TIMEOUT)
        .between_bytes_timeout(HTTP_BETWEEN_BYTES_TIMEOUT);
    request = config.apply_auth_header(request);
    if let Some(body) = body {
        request = request
            .header("content-type", b"application/json".to_vec())
            .body(body);
    }

    mitb_sdk::http::send(request).await
}

async fn repository_has_changes() -> Result<bool, String> {
    let output = run_process(
        "bash",
        vec![
            "-lc".to_string(),
            "if git diff --quiet && git diff --cached --quiet && [ -z \"$(git ls-files --others --exclude-standard)\" ]; then echo clean; else echo dirty; fi".to_string(),
        ],
    )
    .await?;
    let text = String::from_utf8(output)
        .map_err(|error| format!("git diff output was not valid utf-8: {error}"))?;
    Ok(text.trim() == "dirty")
}

async fn log_verification_chunk(stream_name: &str, command: &str, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(chunk).into_owned();
    let truncated = mitb_sdk::truncate(text.trim_end_matches('\n'), MAX_VERIFICATION_OUTPUT_BYTES);
    if truncated.trim().is_empty() {
        return;
    }
    let _ = (stream_name, command);
    log::debug!("[verification-output] {truncated}");
}

async fn create_commit_for_comment(comment_id: u64) -> Result<String, String> {
    run_process("git", vec!["add".to_string(), "-A".to_string()]).await?;
    run_process(
        "git",
        vec![
            "commit".to_string(),
            "-m".to_string(),
            format!("chore: Address comment {comment_id}"),
        ],
    )
    .await?;
    read_trimmed_stdout("git", vec!["rev-parse".to_string(), "HEAD".to_string()]).await
}

enum IntegrationOutcome {
    Integrated { landed_commit_sha: String },
    NeedsPrompt(String),
}

async fn checkout_branch(branch: &str) -> Result<(), String> {
    run_process("git", vec!["checkout".to_string(), branch.to_string()])
        .await
        .map(|_| ())
}

async fn fetch_remote_branch(branch: &str) -> Result<(), String> {
    run_process(
        "git",
        vec![
            "fetch".to_string(),
            "origin".to_string(),
            branch.to_string(),
        ],
    )
    .await
    .map(|_| ())
}

async fn rebase_onto_remote_branch(branch: &str) -> Result<(), String> {
    run_process(
        "git",
        vec!["rebase".to_string(), format!("origin/{branch}")],
    )
    .await
    .map(|_| ())
}

async fn rebase_in_progress() -> Result<bool, String> {
    let output = run_process(
        "bash",
        vec![
            "-lc".to_string(),
            "if [ -d \"$(git rev-parse --git-path rebase-merge)\" ] || [ -d \"$(git rev-parse --git-path rebase-apply)\" ]; then echo yes; else echo no; fi".to_string(),
        ],
    )
    .await?;
    Ok(String::from_utf8_lossy(output.as_slice()).trim() == "yes")
}

async fn list_rebase_conflict_paths() -> Result<Vec<String>, String> {
    let output = run_process(
        "git",
        vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--diff-filter=U".to_string(),
        ],
    )
    .await?;
    let text = String::from_utf8(output)
        .map_err(|error| format!("conflict file output was not valid utf-8: {error}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

async fn push_head_to_remote_branch(branch: &str) -> Result<(), String> {
    run_process(
        "git",
        vec![
            "push".to_string(),
            "origin".to_string(),
            format!("HEAD:{branch}"),
        ],
    )
    .await
    .map(|_| ())
}

async fn ensure_responder_branch_before_thread(config: &GitConfig) -> Result<(), String> {
    let IntegrationMode::Parallel(parallel) = &config.integration_mode else {
        return Ok(());
    };

    let current_branch = current_branch_name().await?;
    if !should_checkout_responder_branch(
        current_branch.as_str(),
        parallel.responder_branch.as_str(),
    ) {
        return Ok(());
    }

    checkout_branch(parallel.responder_branch.as_str()).await.map_err(|error| {
        format!(
            "failed checking out responder branch `{}` before starting next thread from branch `{}`: {}",
            parallel.responder_branch, current_branch, error
        )
    })
}

async fn restore_target_branch_after_thread(config: &GitConfig) -> Result<(), String> {
    let IntegrationMode::Parallel(parallel) = &config.integration_mode else {
        return Ok(());
    };

    match checkout_branch(parallel.target_branch.as_str()).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if is_branch_checked_out_in_other_worktree_error(error.as_str()) {
                log::warn!(
                    "Unable to checkout target branch `{}` after finishing a thread because another worktree already uses it: {}. Staying on responder branch `{}`.",
                    parallel.target_branch,
                    error,
                    parallel.responder_branch
                );
                return Ok(());
            }
            Err(format!(
                "failed checking out target branch `{}` after completing thread: {}",
                parallel.target_branch, error
            ))
        }
    }
}

fn should_checkout_responder_branch(current_branch: &str, responder_branch: &str) -> bool {
    current_branch != responder_branch
}

fn is_non_fast_forward_push_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("non-fast-forward")
        || normalized.contains("fetch first")
        || normalized.contains("remote contains work")
        || (normalized.contains("[rejected]") && normalized.contains("failed to push"))
}

fn is_branch_checked_out_in_other_worktree_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("already checked out at")
        || normalized.contains("is already used by worktree")
        || (normalized.contains("cannot force update the branch")
            && normalized.contains("checked out at"))
}

fn is_rebase_conflict_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("conflict")
        || normalized.contains("could not apply")
        || normalized.contains("resolve all conflicts manually")
}

async fn integration_conflict_prompt(
    active: &ActiveThread,
    parallel: &ParallelGitConfig,
    error: &str,
) -> Result<String, String> {
    let conflicts = list_rebase_conflict_paths().await?;
    let mut prompt = format!(
        "Integration conflict while rebasing responder branch `{}` onto `origin/{}` for comment {}.\n\
Error:\n{}\n\n\
Resolve the conflicts on the current branch, then continue the rebase (`git add <files>` and `git rebase --continue`).\n\
After resolving, continue addressing the same thread.",
        parallel.responder_branch,
        parallel.target_branch,
        active.comment_id,
        mitb_sdk::truncate(error, 4000)
    );
    if !conflicts.is_empty() {
        prompt.push_str("\n\nConflicted files:\n");
        for path in conflicts {
            prompt.push_str(format!("- {path}\n").as_str());
        }
    }
    Ok(prompt)
}

fn integration_push_race_prompt(comment_id: u64, target_branch: &str, error: &str) -> String {
    format!(
        "Repeated push races while integrating comment {comment_id} into `{target_branch}`.\n\
Automatic retries were exhausted.\n\
Error:\n{}\n\n\
Reconcile the branch state from the current branch (fetch/rebase as needed), then continue.",
        mitb_sdk::truncate(error, 4000)
    )
}

async fn push_current_branch() -> Result<(), String> {
    let branch = read_trimmed_stdout(
        "git",
        vec![
            "rev-parse".to_string(),
            "--abbrev-ref".to_string(),
            "HEAD".to_string(),
        ],
    )
    .await?;
    if branch == "HEAD" {
        return Err(String::from(
            "cannot push from detached HEAD; checkout a branch first",
        ));
    }
    push_head_to_remote_branch(branch.as_str()).await
}

async fn read_trimmed_stdout(name: &str, args: Vec<String>) -> Result<String, String> {
    let output = run_process(name, args).await?;
    let text = String::from_utf8(output)
        .map_err(|error| format!("output from `{name}` was not valid utf-8: {error}"))?;
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        Err(format!("output from `{name}` was empty"))
    } else {
        Ok(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GitConfig, GitProvider, IntegrationMode, LeaseConfig, NotePosition, ReplyArtifact,
        ResponderMode, ThreadLease, addressed_commit_reply, build_thread_prompt, describe_position,
        extract_reply, is_branch_checked_out_in_other_worktree_error,
        is_non_fast_forward_push_error, is_rebase_conflict_error, map_reaction_for_provider,
        missing_reply_prompt, normalize_project_path, parse_bool_env_value, parse_pr_url,
        parse_unresolved_discussion, percent_addressed, project_path_is_suffix,
        sanitize_lease_component, sanitize_reply_text, scope_terminal_output_to_prompt,
        should_checkout_responder_branch, url_encode_component, with_git_reply_preamble,
    };
    use core::time::Duration;
    use serde_json::json;
    use std::path::Path;

    fn test_git_config() -> GitConfig {
        GitConfig {
            provider: GitProvider::Gitlab,
            base_url: String::from("https://example.com"),
            token: String::from("token"),
            project: String::from("root/repo"),
            project_api_id: None,
            pr_iid: String::from("1"),
            verification_command: None,
            verification_timeout: Duration::from_secs(1),
            responder_mode: ResponderMode::ReadWrite,
            clear_cmd: None,
            drip: None,
            allowed_users: None,
            lease: None,
            integration_mode: IntegrationMode::DirectPush,
        }
    }

    #[test]
    fn url_encoding_escapes_project_path_slash() {
        assert_eq!(
            url_encode_component("group/project"),
            "group%2Fproject".to_string()
        );
    }

    #[test]
    fn reply_extraction_returns_last_reply_tag() {
        let text = "<reply>first</reply>\n\n<reply>second</reply>\n";
        let reply = extract_reply(text).unwrap();
        assert_eq!(reply, Some(String::from("second")));
    }

    #[test]
    fn reply_extraction_handles_partial_closing_tag_fragments() {
        let text = "<reply>\nGood suggestion — approved_step should be an enum.\n</reply\nthis trailing text should be ignored\n</reply>\n";
        let reply = extract_reply(text).unwrap().unwrap();
        assert!(reply.contains("approved_step should be an enum"));
        assert!(!reply.contains("</reply"));
        assert!(!reply.contains("ignored"));
    }

    #[test]
    fn scope_terminal_output_to_prompt_discards_prior_threads() {
        let output =
            "old stuff\n<reply>stale</reply>\nPrompt token: token-abc\n<reply>fresh</reply>\n";
        let scoped = scope_terminal_output_to_prompt(output, "token-abc");
        let reply = extract_reply(scoped).unwrap().unwrap();
        assert_eq!(reply, "fresh");
    }

    #[test]
    fn scoped_extraction_returns_none_without_current_reply() {
        let output = "Prompt token: token-abc\nnoise only\n";
        let scoped = scope_terminal_output_to_prompt(output, "token-abc");
        let reply = extract_reply(scoped).unwrap();
        assert!(reply.is_none());
    }

    #[test]
    fn unresolved_discussion_parser_extracts_comment_id_and_summary() {
        let discussion = json!({
            "id": "abc123",
            "resolvable": true,
            "resolved": false,
            "notes": [
                {
                    "id": 77,
                    "body": "please update this",
                    "resolvable": true,
                    "resolved": false,
                    "author": { "username": "reviewer" }
                }
            ]
        });

        let parsed = parse_unresolved_discussion(&discussion).unwrap();
        assert_eq!(parsed.discussion_id, "abc123");
        assert_eq!(parsed.comment_id, 77);
        assert_eq!(parsed.notes.len(), 1);
        assert_eq!(parsed.notes[0].body, "please update this");
    }

    #[test]
    fn parse_pr_url_extracts_gitlab_base_project_and_iid() {
        let parsed =
            parse_pr_url("http://localhost:55001/root/dragon-backend/-/merge_requests/1").unwrap();
        assert_eq!(parsed.base_url, "http://localhost:55001");
        assert_eq!(parsed.project_path, "root/dragon-backend");
        assert_eq!(parsed.pr_iid, "1");
        assert_eq!(parsed.provider, GitProvider::Gitlab);
    }

    #[test]
    fn parse_pr_url_extracts_github_owner_repo_and_iid() {
        let parsed = parse_pr_url("https://github.com/root/dragon-backend/pull/7").unwrap();
        assert_eq!(parsed.base_url, "https://api.github.com");
        assert_eq!(parsed.project_path, "root/dragon-backend");
        assert_eq!(parsed.pr_iid, "7");
        assert_eq!(
            parsed.provider,
            GitProvider::Github {
                owner: String::from("root"),
                repo: String::from("dragon-backend"),
            }
        );
    }

    #[test]
    fn parse_pr_url_rejects_non_pr_url() {
        assert!(parse_pr_url("http://localhost:55001/root/dragon-backend").is_err());
    }

    #[test]
    fn parse_pr_url_decodes_percent_encoded_gitlab_project_path() {
        let parsed =
            parse_pr_url("http://localhost:55001/nicksenger%2Fokrs/-/merge_requests/1").unwrap();
        assert_eq!(parsed.project_path, "nicksenger/okrs");
    }

    #[test]
    fn parse_pr_url_rejects_non_github_pull_style_url() {
        assert!(parse_pr_url("http://localhost:3000/root/dragon-backend/pulls/7").is_err());
    }

    #[test]
    fn describe_position_includes_path_and_lines() {
        let description = describe_position(&NotePosition {
            path: Some(String::from("src/lib.rs")),
            new_line: Some(21),
            old_line: None,
            start_line: Some(20),
            end_line: Some(24),
            line_code: Some(String::from("abc_123_456")),
            position_type: Some(String::from("text")),
        });
        assert!(description.contains("path=src/lib.rs"));
        assert!(description.contains("new_line=21"));
        assert!(description.contains("line_range=20-24"));
        assert!(description.contains("position_type=text"));
        assert!(description.contains("line_code=abc_123_456"));
    }

    #[test]
    fn addressed_commit_reply_includes_optional_reply_body() {
        assert_eq!(
            addressed_commit_reply("abc123", Some("Done and verified.")),
            "Addressed in abc123\n\nDone and verified."
        );
        assert_eq!(
            addressed_commit_reply("abc123", None),
            "Addressed in abc123"
        );
    }

    #[test]
    fn percent_addressed_matches_thread_progression_example() {
        assert!((percent_addressed(0, 4) - 0.0).abs() < 1e-12);
        assert!((percent_addressed(1, 3) - 0.25).abs() < 1e-12);
        assert!((percent_addressed(2, 2) - 0.5).abs() < 1e-12);
        assert!((percent_addressed(2, 3) - 0.4).abs() < 1e-12);
        assert!((percent_addressed(3, 2) - 0.6).abs() < 1e-12);
        assert!((percent_addressed(5, 0) - 1.0).abs() < 1e-12);
        assert!((percent_addressed(5, 1) - 0.8333333333333334).abs() < 1e-12);
    }

    #[test]
    fn sanitize_reply_text_strips_terminal_chrome_and_ansi_artifacts() {
        let raw = "Great call — I refactored [48;2;51;52;70mMockSlackClient[39m to share state.\n\
┌────────────────────────────────────────────────────────────────────┐\n\
│ [2m→ [22m[7mA[27m[90mdd a follow-up[39m                                   [90mctrl+c to stop[39m │\n\
└────────────────────────────────────────────────────────────────────┘\n\
[2K[1A[2K[1A[2K[G  ⬡ Generating.. 63.91k tokens\n\
Validated with cargo test -p slack_client_mock.";
        let cleaned = sanitize_reply_text(raw);
        assert!(cleaned.contains("Great call — I refactored MockSlackClient to share state."));
        assert!(cleaned.contains("Validated with cargo test -p slack_client_mock."));
        assert!(!cleaned.contains("Add a follow-up"));
        assert!(!cleaned.contains("[48;2;51;52;70m"));
        assert!(!cleaned.contains("[2K[1A"));
    }

    #[test]
    fn sanitize_reply_text_strips_prompt_echo_noise() {
        let raw = "\", include only the reviewer-facing         │\n\
Cursor Agent v2026.03.25-933d5a6\n\
~/projects/localdragonbackend · autorelease/dragon-worker\n\
Address Git PR comment 201 in read-only mode.\n\
Thread details:\n\
GitLab discussion 52ae4ea9b66fedc00c9d54596332a3c3210fca5c\n\
\n\
note 201 by @root (resolvable=true, resolved=false):…\n\
\n\
⬢ Generating.\n\
<\n\
Great suggestion — approved_step is currently a free-form\n\
&'static str.\n\
This policy is running in read-only mode, so I can’t modify code\n\
in this repository.\",";
        let cleaned = sanitize_reply_text(raw);
        assert!(cleaned.starts_with("Great suggestion"));
        assert!(cleaned.contains("read-only mode"));
        assert!(!cleaned.contains("Cursor Agent"));
        assert!(!cleaned.contains("Address Git PR comment"));
        assert!(!cleaned.starts_with('"'));
        assert!(!cleaned.ends_with('"'));
    }

    #[test]
    fn sanitize_reply_text_dedupes_partial_fragments_and_prefix_lines() {
        let raw = "Good\n\
Good suggestion — approved_step can be made a proper enum\n\
to improve type safety and avoid invalid string values. This\n\
policy is running in read-only mode, so I can’t modify code in\n\
this repository from here.</reply\n\
this repository from here.";
        let cleaned = sanitize_reply_text(raw);
        assert!(cleaned.starts_with("Good suggestion"));
        assert!(!cleaned.contains("</reply"));
        assert_eq!(cleaned.matches("this repository from here.").count(), 1);
    }

    #[test]
    fn sanitize_reply_text_collapses_stream_prefix_redraw_lines() {
        let raw = "Yes, that makes\n\
Yes, that makes sense — approved_step is a good candidate\n\
for a dedicated enum to make the allowed values explicit and\n\
type-safe. This policy\n\
type-safe. This policy is running in read-only mode, so I can’t\n\
modify the code here, but the intended change would be to replace\n\
the `&'\n\
the &'static str field in ApprovalTemplate and\n\
the &'static str field in ApprovalTemplate and ApprovalPlan with\n\
an enum and only convert to string at output boundaries\n\
(Slack/event\n\
(Slack/event text).";
        let cleaned = sanitize_reply_text(raw);
        assert!(!cleaned.contains("Yes, that makes\nYes, that makes sense"));
        assert!(!cleaned.contains("type-safe. This policy\ntype-safe. This policy"));
        assert!(!cleaned.contains("the `&'\nthe &'static"));
        assert!(!cleaned.contains("(Slack/event\n(Slack/event text)."));
        assert!(cleaned.contains("Yes, that makes sense"));
        assert!(cleaned.contains("type-safe. This policy is running"));
        assert!(cleaned.contains("the &'static str field in ApprovalTemplate and ApprovalPlan"));
        assert!(cleaned.contains("(Slack/event text)."));
    }

    #[test]
    fn trim_wrapping_reply_artifacts_removes_quote_comma_wrappers() {
        assert_eq!(
            ThreadLease::trim_wrapping_reply_artifacts("\", hello world\","),
            "hello world"
        );
    }

    #[test]
    fn read_only_prompt_requires_reply_and_no_edits() {
        let prompt =
            build_thread_prompt("please update this function", 42, ResponderMode::ReadOnly);
        assert!(prompt.contains("read-only mode"));
        assert!(prompt.contains("Do not make any code changes"));
        assert!(prompt.contains("reviewer-facing reply"));
    }

    #[test]
    fn read_only_missing_reply_prompt_mentions_cannot_modify() {
        let prompt = missing_reply_prompt("nit: please refactor", 99, ResponderMode::ReadOnly);
        assert!(prompt.contains("Read-only mode is enabled"));
        assert!(prompt.contains("cannot make modifications"));
    }

    #[test]
    fn parse_reply_candidate_accepts_plain_text_file_payload() {
        let reply = ReplyArtifact::parse_reply_candidate("Looks good to me.").unwrap();
        assert_eq!(reply, Some(String::from("Looks good to me.")));
    }

    #[test]
    fn parse_reply_candidate_accepts_tagged_file_payload() {
        let reply = ReplyArtifact::parse_reply_candidate("<reply>Done.</reply>").unwrap();
        assert_eq!(reply, Some(String::from("Done.")));
    }

    #[test]
    fn parse_bool_env_value_accepts_truthy_values() {
        assert!(parse_bool_env_value("1"));
        assert!(parse_bool_env_value("true"));
        assert!(parse_bool_env_value("YES"));
        assert!(parse_bool_env_value("on"));
        assert!(!parse_bool_env_value("0"));
        assert!(!parse_bool_env_value("false"));
        assert!(!parse_bool_env_value("random"));
    }

    #[test]
    fn push_error_detector_identifies_non_fast_forward_rejections() {
        assert!(is_non_fast_forward_push_error(
            "remote: error: failed to push some refs\n ! [rejected] HEAD -> feature (non-fast-forward)"
        ));
        assert!(is_non_fast_forward_push_error(
            "Updates were rejected because the remote contains work that you do not have locally."
        ));
        assert!(!is_non_fast_forward_push_error(
            "fatal: authentication failed"
        ));
    }

    #[test]
    fn rebase_error_detector_identifies_conflict_messages() {
        assert!(is_rebase_conflict_error(
            "error: could not apply abc123... chore: Address comment\nResolve all conflicts manually."
        ));
        assert!(is_rebase_conflict_error(
            "CONFLICT (content): Merge conflict in src/lib.rs"
        ));
        assert!(!is_rebase_conflict_error("fatal: invalid upstream"));
    }

    #[test]
    fn checkout_error_detector_identifies_branch_held_by_other_worktree() {
        assert!(is_branch_checked_out_in_other_worktree_error(
            "fatal: 'main' is already checked out at '/tmp/repo-main'"
        ));
        assert!(is_branch_checked_out_in_other_worktree_error(
            "fatal: branch is already used by worktree at '/tmp/repo-main'"
        ));
        assert!(!is_branch_checked_out_in_other_worktree_error(
            "fatal: pathspec 'missing-branch' did not match any file(s) known to git"
        ));
    }

    #[test]
    fn responder_branch_checkout_detector_only_switches_when_needed() {
        assert!(should_checkout_responder_branch("main", "mitb/parallel/mr-1/worker"));
        assert!(!should_checkout_responder_branch(
            "mitb/parallel/mr-1/worker",
            "mitb/parallel/mr-1/worker"
        ));
    }

    #[test]
    fn sanitize_lease_component_replaces_non_path_safe_characters() {
        assert_eq!(
            sanitize_lease_component("discussion/abc:123 with spaces"),
            "discussion-abc-123-with-spaces"
        );
        assert_eq!(sanitize_lease_component("***"), "unknown");
    }

    #[test]
    fn shared_root_guest_mount_uses_mitb_shared_for_absolute_host_path() {
        assert_eq!(
            LeaseConfig::shared_root_guest_mount("/Users/example/.mitb/shared"),
            "mitb-shared"
        );
        assert_eq!(
            LeaseConfig::shared_root_guest_mount("./mitb-shared/"),
            "mitb-shared"
        );
    }

    #[test]
    fn normalize_lease_root_for_guest_maps_host_shared_prefix_to_guest_mount() {
        assert_eq!(
            LeaseConfig::normalize_root_for_guest(
                "/Users/example/.mitb/shared/leases/project-a",
                Some("/Users/example/.mitb/shared")
            ),
            "mitb-shared/leases/project-a"
        );
        assert_eq!(
            LeaseConfig::normalize_root_for_guest(
                "mitb-shared/leases/project-a",
                Some("/Users/example/.mitb/shared")
            ),
            "mitb-shared/leases/project-a"
        );
    }

    #[test]
    fn parse_thread_lease_record_reads_expected_fields() {
        let worker_id = String::from("2d931510-d99f-494a-8c67-87feb05e1594");
        let attempt_id = String::from("f47ac10b-58cc-4372-a567-0e02b2c3d479");
        let value = json!({
            "worker_id": worker_id.as_str(),
            "attempt_id": attempt_id,
            "fencing_token": 9,
            "acquired_at": 10,
            "expires_at": 20,
            "heartbeat_at": 15
        });
        let lease = test_git_config()
            .parse_thread_lease_record(
                &value,
                Path::new("mitb-shared/leases/thread-1"),
                Path::new("mitb-shared/leases/thread-1/lease.json"),
            )
            .expect("lease should parse");
        assert_eq!(lease.worker_id.to_string(), worker_id);
        assert_eq!(lease.attempt_id.to_string(), attempt_id);
        assert_eq!(lease.fencing_token, 9);
        assert_eq!(lease.acquired_at, 10);
        assert_eq!(lease.expires_at, 20);
        assert_eq!(lease.heartbeat_at, 15);
    }

    #[test]
    fn parse_thread_lease_record_rejects_non_uuid_worker_ids() {
        let value = json!({
            "worker_id": "worker-1",
            "attempt_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "fencing_token": 9,
            "acquired_at": 10,
            "expires_at": 20,
            "heartbeat_at": 15
        });
        let lease = test_git_config().parse_thread_lease_record(
            &value,
            Path::new("mitb-shared/leases/thread-1"),
            Path::new("mitb-shared/leases/thread-1/lease.json"),
        );
        assert!(lease.is_none(), "non-UUID worker ids should be rejected");
    }

    #[test]
    fn parse_thread_lease_record_rejects_non_uuid_attempt_ids() {
        let value = json!({
            "worker_id": "2d931510-d99f-494a-8c67-87feb05e1594",
            "attempt_id": "attempt-legacy",
            "fencing_token": 9,
            "acquired_at": 10,
            "expires_at": 20,
            "heartbeat_at": 15
        });
        let lease = test_git_config().parse_thread_lease_record(
            &value,
            Path::new("mitb-shared/leases/thread-1"),
            Path::new("mitb-shared/leases/thread-1/lease.json"),
        );
        assert!(lease.is_none(), "non-UUID attempt ids should be rejected");
    }

    #[test]
    fn git_reply_preamble_is_added_once() {
        let reply = with_git_reply_preamble("Addressed in abc123");
        assert!(
            reply.starts_with("[This is an automated reply from the **_Man in the Box_**]\n\n")
        );
        assert!(reply.ends_with("Addressed in abc123"));

        let second_pass = with_git_reply_preamble(reply.as_str());
        assert_eq!(reply, second_pass);
    }

    #[test]
    fn map_reaction_for_provider_keeps_gitlab_shortcodes() {
        assert_eq!(
            map_reaction_for_provider(&GitProvider::Gitlab, ":takeout_box:"),
            Some(String::from("takeout_box"))
        );
        assert_eq!(
            map_reaction_for_provider(&GitProvider::Gitlab, "ballot_box_with_check"),
            Some(String::from("ballot_box_with_check"))
        );
    }

    #[test]
    fn map_reaction_for_provider_skips_unsupported_github_reactions() {
        let provider = GitProvider::Github {
            owner: String::from("root"),
            repo: String::from("dragon-backend"),
        };
        assert_eq!(
            map_reaction_for_provider(&provider, ":eye:"),
            Some(String::from("eyes"))
        );
        assert_eq!(map_reaction_for_provider(&provider, ":takeout_box:"), None);
        assert_eq!(
            map_reaction_for_provider(&provider, ":ballot_box_with_check:"),
            Some(String::from("rocket"))
        );
    }

    #[test]
    fn normalize_project_path_collapses_empty_segments() {
        assert_eq!(normalize_project_path("/root//repo/"), "root/repo");
    }

    #[test]
    fn project_path_suffix_match_accepts_prefixed_paths() {
        assert!(project_path_is_suffix(
            "gitlab/root/dragon-backend",
            "root/dragon-backend"
        ));
        assert!(!project_path_is_suffix(
            "root/dragon-backend",
            "root/dragon-worker"
        ));
    }

    #[test]
    fn gitlab_project_path_segment_prefers_resolved_project_id() {
        let mut config = test_git_config();
        assert_eq!(config.gitlab_project_path_segment(), "root/repo");
        config.project_api_id = Some(String::from("1234"));
        assert_eq!(config.gitlab_project_path_segment(), "1234");
    }
}

bindings::export_policy!(GitResponder);
