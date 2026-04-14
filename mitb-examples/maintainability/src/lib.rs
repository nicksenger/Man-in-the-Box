mitb_sdk::policy_prelude!("maintainability");

use std::collections::HashSet;

const RUST_LANGUAGE: &str = "rust";
const IGNORED_PATHS_ENV: &str = "MITB_MAINTAINABILITY_IGNORED_PATHS";
const CYCLOMATIC_QUERY: &str = r#"
[
  (if_expression)
  (match_expression)
  (for_expression)
  (while_expression)
  (loop_expression)
] @decision
"#;
const HALSTEAD_QUERY: &str = r#"
[
  (binary_expression)
  (unary_expression)
  (assignment_expression)
  (call_expression)
  (field_expression)
  (index_expression)
  (if_expression)
  (match_expression)
  (for_expression)
  (while_expression)
  (loop_expression)
  (return_expression)
  (break_expression)
  (continue_expression)
] @operator

[
  (identifier)
  (integer_literal)
  (float_literal)
  (char_literal)
  (string_literal)
] @operand
"#;
const MCTS_MAX_DEPTH: usize = 32;

#[derive(Default)]
struct Maintainability {
    action_index: u64,
    cyclomatic_query_id: Option<u32>,
    halstead_query_id: Option<u32>,
    state: PolicyState,
}

struct SearchState {
    navigator: mitb_sdk::search::mcts::PendingTreeSearch,
    best_reward_commit: Option<BestRewardCommit>,
}

struct BestRewardCommit {
    commit: String,
    reward: f64,
}

#[derive(Default)]
struct PolicyState {
    session_id: Option<String>,
    created_branch_count: u64,
    search: Option<SearchState>,
}

impl Maintainability {
    fn ignored_paths_from_env() -> Vec<String> {
        let environment = bindings::wasi::cli::environment::get_environment();
        for (key, value) in environment {
            if key == IGNORED_PATHS_ENV {
                return Self::parse_ignored_paths(value.as_str());
            }
        }
        Vec::new()
    }

    fn parse_ignored_paths(raw: &str) -> Vec<String> {
        let mut parsed = Vec::<String>::new();
        for part in raw.split([',', ';', '\n']) {
            if let Some(path) = Self::normalize_path(part)
                && !parsed.contains(&path)
            {
                parsed.push(path);
            }
        }
        parsed
    }

    fn normalize_path(path: &str) -> Option<String> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return None;
        }

        if trimmed == "." {
            Some(String::from("."))
        } else {
            let slash_normalized = trimmed.replace('\\', "/");
            if slash_normalized.starts_with('/') {
                return None;
            }

            let mut components = Vec::<&str>::new();
            for component in slash_normalized.split('/') {
                if component.is_empty() || component == "." {
                    continue;
                }

                if component == ".." {
                    if components
                        .last()
                        .copied()
                        .is_some_and(|segment| segment != "..")
                    {
                        let _ = components.pop();
                    } else {
                        components.push(component);
                    }
                    continue;
                }

                components.push(component);
            }

            if components.is_empty() {
                Some(String::from("."))
            } else {
                Some(components.join("/"))
            }
        }
    }

    fn path_is_ignored(path: &str, ignored_paths: &[String]) -> bool {
        let Some(normalized) = Self::normalize_path(path) else {
            return false;
        };
        ignored_paths.iter().any(|ignored| {
            ignored == "."
                || normalized == *ignored
                || normalized
                    .strip_prefix(ignored.as_str())
                    .map(|suffix| suffix.starts_with('/'))
                    .unwrap_or(false)
        })
    }

    fn ignored_paths_prompt_clause(ignored_paths: &[String]) -> String {
        if ignored_paths.is_empty() {
            String::from("Ignored paths: none.")
        } else {
            format!("Ignored paths: {}.", ignored_paths.join(", "))
        }
    }

    fn query_error_string(error: bindings::mitb::treesitter::api::QueryError) -> &'static str {
        match error {
            bindings::mitb::treesitter::api::QueryError::UnsupportedLanguage => {
                "unsupported language"
            }
            bindings::mitb::treesitter::api::QueryError::InvalidQuery => "invalid query",
            bindings::mitb::treesitter::api::QueryError::UnknownTree => "unknown tree",
            bindings::mitb::treesitter::api::QueryError::UnknownQuery => "unknown query",
            bindings::mitb::treesitter::api::QueryError::Internal => "internal query error",
        }
    }

    fn parse_error_string(error: bindings::mitb::treesitter::api::ParseError) -> &'static str {
        match error {
            bindings::mitb::treesitter::api::ParseError::UnsupportedLanguage => {
                "unsupported language"
            }
            bindings::mitb::treesitter::api::ParseError::ParseFailed => "parse failed",
            bindings::mitb::treesitter::api::ParseError::Internal => "internal parse error",
        }
    }

    fn compute_maintainability_index(
        halstead_volume: f64,
        cyclomatic_complexity: f64,
        loc: f64,
    ) -> f64 {
        let safe_volume = halstead_volume.max(1.0);
        let safe_loc = loc.max(1.0);
        let raw = 171.0
            - (5.2 * safe_volume.ln())
            - (0.23 * cyclomatic_complexity)
            - (16.2 * safe_loc.ln());
        raw * 100.0 / 171.0
    }

    fn count_lines(text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let newline_count = text.bytes().filter(|byte| *byte == b'\n').count() as u64;
        if text.as_bytes().last() == Some(&b'\n') {
            newline_count
        } else {
            newline_count + 1
        }
    }

    async fn ensure_queries(&mut self) -> Result<(u32, u32), String> {
        if self.cyclomatic_query_id.is_none() {
            self.cyclomatic_query_id = Some(
                bindings::mitb::treesitter::api::query_compile(RUST_LANGUAGE, CYCLOMATIC_QUERY)
                    .map_err(|error| {
                        format!(
                            "failed compiling cyclomatic query: {}",
                            Self::query_error_string(error)
                        )
                    })?,
            );
        }
        if self.halstead_query_id.is_none() {
            self.halstead_query_id = Some(
                bindings::mitb::treesitter::api::query_compile(RUST_LANGUAGE, HALSTEAD_QUERY)
                    .map_err(|error| {
                        format!(
                            "failed compiling halstead query: {}",
                            Self::query_error_string(error)
                        )
                    })?,
            );
        }

        match (self.cyclomatic_query_id, self.halstead_query_id) {
            (Some(cyclomatic_query_id), Some(halstead_query_id)) => {
                Ok((cyclomatic_query_id, halstead_query_id))
            }
            _ => Err(String::from("query IDs were not initialized")),
        }
    }

    async fn ensure_search_state_recorded(&mut self) -> Result<(), String> {
        if self.state.search.is_some() {
            return Ok(());
        }

        let snapshot = current_git_snapshot().await?;
        self.state.search = Some(SearchState {
            navigator: mitb_sdk::search::mcts::PendingTreeSearch::new(snapshot.sha),
            best_reward_commit: None,
        });
        Ok(())
    }

    fn ensure_session_id_recorded(&mut self) {
        if self.state.session_id.is_none() {
            self.state.session_id = Some(generate_session_id());
        }
    }

    fn search_state_mut(&mut self) -> Result<&mut SearchState, String> {
        self.state
            .search
            .as_mut()
            .ok_or_else(|| String::from("missing search state"))
    }

    fn next_search_branch_name(&mut self) -> Result<String, String> {
        let session_id = self
            .state
            .session_id
            .as_deref()
            .ok_or_else(|| String::from("missing session id for branch selection"))?;
        self.state.created_branch_count = self.state.created_branch_count.saturating_add(1);
        Ok(format!(
            "mitb/maintainability-{session_id}-{}",
            self.state.created_branch_count
        ))
    }

    fn record_best_reward_commit(
        &mut self,
        commit: &str,
        reward: f64,
    ) -> Result<(String, f64), String> {
        let search_state = self.search_state_mut()?;
        let replace_best = match search_state.best_reward_commit.as_ref() {
            Some(best) => reward > best.reward,
            None => true,
        };
        if replace_best {
            search_state.best_reward_commit = Some(BestRewardCommit {
                commit: commit.to_string(),
                reward,
            });
        }

        let best = search_state
            .best_reward_commit
            .as_ref()
            .ok_or_else(|| String::from("missing best reward commit state"))?;
        Ok((best.commit.clone(), best.reward))
    }

    async fn commit_iteration_snapshot(
        &mut self,
        mi: f64,
    ) -> Result<mitb_sdk::git::GitSnapshot, String> {
        let repo = git_repo();
        let run =
            |name: String, args: Vec<String>| async move { run_process(name.as_str(), args).await };
        repo.add_all(&run).await?;
        let sha = repo
            .commit_all(
                &run,
                format!(
                    "maintainability iteration {} mi {:.2}",
                    self.action_index, mi
                )
                .as_str(),
                true,
            )
            .await?;
        let branch = repo.current_branch(&run).await?;
        Ok(mitb_sdk::git::GitSnapshot { sha, branch })
    }

    async fn checkout_selected_commit(&mut self, commit: &str) -> Result<String, String> {
        let repo = git_repo();
        let run =
            |name: String, args: Vec<String>| async move { run_process(name.as_str(), args).await };
        let branch_name = self.next_search_branch_name()?;
        repo.switch_create_or_reset(&run, branch_name.as_str(), commit)
            .await?;
        Ok(branch_name)
    }

    async fn navigate_reward_landscape(
        &mut self,
        mi: f64,
        reward: f64,
        rust_loc: u64,
        snapshot: &mitb_sdk::git::GitSnapshot,
    ) -> Result<(), String> {
        let reward = mitb_sdk::search::mcts::normalize_reward(reward);
        let step = {
            let search_state = self.search_state_mut()?;
            search_state.navigator.backpropagate_and_select(
                snapshot.sha.as_str(),
                reward,
                MCTS_MAX_DEPTH,
            )?
        };
        let selected_commit = step.selected_key;
        let selected_path = step.selected_path;
        let (best_reward_commit, best_reward) =
            self.record_best_reward_commit(snapshot.sha.as_str(), reward)?;

        let branch_name = self
            .checkout_selected_commit(selected_commit.as_str())
            .await?;
        log::info!(
            "mi={mi:.2} reward={reward:.4} rust_loc={rust_loc} selected_commit={} selected_path={} branch={} best_reward_commit={} best_reward={best_reward:.4}",
            selected_commit,
            selected_path.join(" -> "),
            branch_name,
            best_reward_commit
        );
        Ok(())
    }
}

impl Policy for Maintainability {
    async fn act(&mut self, _contents: String) -> ActionResult {
        self.action_index = self.action_index.saturating_add(1);
        let (cyclomatic_query_id, halstead_query_id) = self.ensure_queries().await?;
        self.ensure_session_id_recorded();
        self.ensure_search_state_recorded().await?;
        let ignored_paths = Self::ignored_paths_from_env();
        let ignored_prompt_clause = Self::ignored_paths_prompt_clause(ignored_paths.as_slice());

        let files = mitb_sdk::fs::find("*.rs", mitb_sdk::fs::FindOptions::default())
            .await?
            .into_iter()
            .filter(|file| !Self::path_is_ignored(file.path.as_str(), ignored_paths.as_slice()))
            .collect::<Vec<_>>();
        if files.is_empty() {
            let mi = 100.0;
            let reward = 1.0;
            let snapshot = self.commit_iteration_snapshot(mi).await?;
            report_reward!(reward);
            log::info!(
                "mi=100.00 reward=1.0000 loc=0 cc=0.00 halstead_volume=0.00 files=0 ignored_paths={}",
                if ignored_paths.is_empty() {
                    String::from("none")
                } else {
                    ignored_paths.join(",")
                }
            );
            self.navigate_reward_landscape(mi, reward, 0, &snapshot)
                .await?;
            return prompt!(format!(
                "The maintainability index of this application is currently 100.00. Refactor to improve the maintainability index. {ignored_prompt_clause}"
            ));
        }

        let mut analyzed_files = 0_u64;
        let mut mi_sum = 0.0_f64;
        let mut cyclomatic_sum = 0.0_f64;
        let mut halstead_volume_sum = 0.0_f64;
        let mut loc_sum = 0.0_f64;
        let mut project_loc = 0_u64;

        for file in files {
            let source = mitb_sdk::fs::read_text(file.path.as_str()).await?;
            let file_loc_raw = Self::count_lines(source.as_str());
            let file_loc = file_loc_raw as f64;
            project_loc = project_loc.saturating_add(file_loc_raw);
            let tree_id = bindings::mitb::treesitter::api::parse(RUST_LANGUAGE, source.as_str())
                .map_err(|error| {
                    format!(
                        "failed parsing `{}`: {}",
                        file.path,
                        Self::parse_error_string(error)
                    )
                })?;

            let cyclomatic_matches = bindings::mitb::treesitter::api::query_exec(
                cyclomatic_query_id,
                tree_id,
                source.as_str(),
                None,
            )
            .map_err(|error| {
                format!(
                    "failed running cyclomatic query for `{}`: {}",
                    file.path,
                    Self::query_error_string(error)
                )
            })?;

            let halstead_matches = bindings::mitb::treesitter::api::query_exec(
                halstead_query_id,
                tree_id,
                source.as_str(),
                None,
            )
            .map_err(|error| {
                format!(
                    "failed running halstead query for `{}`: {}",
                    file.path,
                    Self::query_error_string(error)
                )
            })?;

            bindings::mitb::treesitter::api::drop_tree(tree_id);

            let cyclomatic_complexity = 1.0 + cyclomatic_matches.len() as f64;
            let mut operators_total = 0_u64;
            let mut operands_total = 0_u64;
            let mut distinct_operators = HashSet::<String>::new();
            let mut distinct_operands = HashSet::<String>::new();

            for matched in halstead_matches {
                for capture in matched.captures {
                    if capture.name == "operator" {
                        operators_total = operators_total.saturating_add(1);
                        let _ = distinct_operators.insert(capture.text);
                    } else if capture.name == "operand" {
                        operands_total = operands_total.saturating_add(1);
                        let _ = distinct_operands.insert(capture.text);
                    }
                }
            }

            let n1 = distinct_operators.len() as f64;
            let n2 = distinct_operands.len() as f64;
            let n_total = operators_total as f64 + operands_total as f64;
            let vocabulary = (n1 + n2).max(2.0);
            let halstead_volume = n_total * vocabulary.log2();
            let file_mi = Maintainability::compute_maintainability_index(
                halstead_volume,
                cyclomatic_complexity,
                file_loc,
            );

            analyzed_files = analyzed_files.saturating_add(1);
            mi_sum += file_mi;
            cyclomatic_sum += cyclomatic_complexity;
            halstead_volume_sum += halstead_volume;
            loc_sum += file_loc.max(1.0);
        }

        let files_count = analyzed_files.max(1) as f64;
        let mi = mi_sum / files_count;
        let avg_cyclomatic = cyclomatic_sum / files_count;
        let avg_halstead_volume = halstead_volume_sum / files_count;
        let avg_loc = loc_sum / files_count;
        let reward = (mi / 100.0).clamp(0.0, 1.0);
        let snapshot = self.commit_iteration_snapshot(mi).await?;

        report_reward!(reward);
        log::info!(
            "mi={mi:.2} reward={reward:.4} loc={project_loc} files={analyzed_files} avg_loc={avg_loc:.2} avg_cc={avg_cyclomatic:.2} avg_halstead_volume={avg_halstead_volume:.2}"
        );
        self.navigate_reward_landscape(mi, reward, project_loc, &snapshot)
            .await?;
        prompt!(format!(
            "The maintainability index of .rs files under the current directory is {mi:.2}. Refactor to improve the maintainability index. {ignored_prompt_clause}"
        ))
    }
}

fn git_repo() -> mitb_sdk::git::GitRepo {
    mitb_sdk::git::GitRepo::new(".")
}

async fn current_git_snapshot() -> Result<mitb_sdk::git::GitSnapshot, String> {
    let repo = git_repo();
    let run =
        |name: String, args: Vec<String>| async move { run_process(name.as_str(), args).await };
    repo.snapshot(&run).await
}

fn generate_session_id() -> String {
    const ALPHANUMERIC: &[u8; 62] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let random = bindings::wasi::random::random::get_random_bytes(6);
    let mut session_id = String::with_capacity(6);
    for byte in random {
        let index = usize::from(byte) % ALPHANUMERIC.len();
        session_id.push(char::from(ALPHANUMERIC[index]));
    }
    session_id
}

#[cfg(test)]
mod tests {
    use super::Maintainability;

    #[test]
    fn normalize_path_collapses_relative_segments() {
        assert_eq!(
            Maintainability::normalize_path("./src/./core/../lib.rs"),
            Some(String::from("src/lib.rs"))
        );
    }

    #[test]
    fn ignored_path_matches_nested_file_recursively() {
        let ignored = vec![String::from("src/generated")];
        assert!(Maintainability::path_is_ignored(
            "src/generated/sub/module.rs",
            ignored.as_slice()
        ));
    }

    #[test]
    fn ignored_path_does_not_match_prefix_only_name() {
        let ignored = vec![String::from("src/gen")];
        assert!(!Maintainability::path_is_ignored(
            "src/generated/module.rs",
            ignored.as_slice()
        ));
    }
}

bindings::export_policy!(Maintainability);
