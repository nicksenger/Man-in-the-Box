use crate::{GIT_REPLY_PREAMBLE, ResponderMode, ThreadLease};

pub(crate) fn build_thread_prompt(
    summary: &str,
    comment_id: u64,
    responder_mode: ResponderMode,
) -> String {
    match responder_mode {
        ResponderMode::ReadOnly => format!(
            "Address Git PR comment {comment_id} in read-only mode.\n\nThread details:\n{summary}\n\
Do not make any code changes in this repository.\n\
Call the `reply` tool from the mitb MCP server to submit your reviewer-facing reply.\n\
If the thread asks for code edits or implementation changes, state that this policy is running in read-only mode and cannot modify code.\n\
"
        ),
        ResponderMode::ReadWrite => format!(
            "Address Git PR comment {comment_id}.\n\nThread details:\n{summary}\n\
If the thread can be interpreted as a request for code changes, implement those changes in this repository.\n\
If the thread cannot be interpreted as a request for code changes, inspect relevant code and call the `reply` tool from the mitb MCP server to submit your reviewer-facing reply.\n\
If you make code changes and also have useful reviewer-facing context, call the `reply` tool from the mitb MCP server to submit it.\n\
"
        ),
    }
}

pub(crate) fn missing_reply_prompt(
    summary: &str,
    comment_id: u64,
    responder_mode: ResponderMode,
) -> String {
    match responder_mode {
        ResponderMode::ReadOnly => format!(
            "No reply text was found for comment {comment_id}.\n\nThread details:\n{summary}\n\
Read-only mode is enabled, so do not make code changes.\n\
Call the `reply` tool from the mitb MCP server to submit your reviewer-facing reply.\n\
If the thread requests code edits, clearly state that this policy is in read-only mode and cannot make modifications."
        ),
        ResponderMode::ReadWrite => format!(
            "No reply text was found for comment {comment_id}.\n\nThread details:\n{summary}\n\
If code changes are still needed, continue implementing them.\n\
If no code changes are needed, call the `reply` tool from the mitb MCP server to submit your reviewer-facing reply."
        ),
    }
}

pub(crate) fn addressed_commit_reply(commit_sha: &str, reply: Option<&str>) -> String {
    let Some(reply) = reply.map(str::trim).filter(|text| !text.is_empty()) else {
        return format!("Addressed in {commit_sha}");
    };
    format!("Addressed in {commit_sha}\n\n{reply}")
}

pub(crate) fn verification_failure_prompt(
    comment_id: u64,
    command: &str,
    error: &str,
    new_thread_replies: Option<&str>,
    retrieval_warning: Option<&str>,
) -> String {
    let mut prompt = format!(
        "Verification failed for comment {comment_id}.\n\
Command: {command}\n\
Error:\n{}\n\n\
Make additional code changes to satisfy the comment and ensure the verification command passes.\n\
If you have reviewer-facing context, call the `reply` tool from the mitb MCP server to submit it.",
        mitb_sdk::truncate(error, 4000),
    );
    if let Some(new_thread_replies) = new_thread_replies {
        prompt.push_str("\n\n");
        prompt.push_str(new_thread_replies.trim());
    }
    if let Some(retrieval_warning) = retrieval_warning {
        prompt.push_str("\n\n");
        prompt.push_str(
            format!(
                "Warning: failed to fetch latest thread replies from the git provider: {}",
                mitb_sdk::truncate(retrieval_warning, 1000)
            )
            .as_str(),
        );
    }
    prompt
}

pub(crate) fn push_failure_prompt(comment_id: u64, commit_sha: &str, error: &str) -> String {
    format!(
        "Created commit {commit_sha} for comment {comment_id}, but failed to push the current branch.\n\
Error:\n{}\n\n\
Resolve the push issue from the current branch so this commit can be reported back to the reviewer.\n\
If you have reviewer-facing context, call the `reply` tool from the mitb MCP server to submit it.",
        mitb_sdk::truncate(error, 4000),
    )
}

pub(crate) fn extract_reply(text: &str) -> Result<Option<String>, String> {
    let Some(reply) = extract_last_reply_block(text) else {
        return Ok(None);
    };
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        let cleaned = sanitize_reply_text(trimmed);
        if cleaned.is_empty() {
            Ok(None)
        } else {
            Ok(Some(cleaned))
        }
    }
}

fn extract_last_reply_block(text: &str) -> Option<String> {
    let mut cursor = 0_usize;
    let mut latest = None::<String>;
    let open_tag = "<reply>";
    let close_tag = "</reply>";

    while let Some(start_rel) = text[cursor..].find(open_tag) {
        let start = cursor + start_rel;
        let line_start = text[..start]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        if !text[line_start..start].trim().is_empty() {
            cursor = start + open_tag.len();
            continue;
        }

        let body_start = start + open_tag.len();
        let full_end_rel = text[body_start..].find(close_tag);
        let partial_end_rel = text[body_start..].find("</reply");
        let Some((end_rel, close_len)) = (match (full_end_rel, partial_end_rel) {
            (Some(full), Some(partial)) => {
                if full <= partial {
                    Some((full, close_tag.len()))
                } else {
                    Some((partial, "</reply".len()))
                }
            }
            (Some(full), None) => Some((full, close_tag.len())),
            (None, Some(partial)) => Some((partial, "</reply".len())),
            (None, None) => None,
        }) else {
            break;
        };
        let body_end = body_start + end_rel;
        let tail_start = body_end + close_len;
        let line_end = text[tail_start..]
            .find('\n')
            .map(|index| tail_start + index)
            .unwrap_or(text.len());
        if !text[tail_start..line_end].trim().is_empty() {
            cursor = body_start;
            continue;
        }

        latest = Some(text[body_start..body_end].to_string());
        cursor = tail_start;
    }

    latest
}

pub(crate) fn sanitize_reply_text(raw: &str) -> String {
    let ansi_stripped = strip_ansi_sequences(raw);
    let mut cleaned_lines = Vec::<String>::new();

    for raw_line in ansi_stripped.replace('\r', "\n").lines() {
        let mut line = strip_bracket_ansi_tokens(raw_line).trim().to_string();
        if line.is_empty() {
            if cleaned_lines
                .last()
                .is_some_and(|previous| previous.is_empty())
            {
                continue;
            }
            cleaned_lines.push(String::new());
            continue;
        }
        if is_terminal_chrome_line(line.as_str()) {
            continue;
        }
        if line == "<" || line == ">" {
            continue;
        }
        line = strip_reply_tag_fragments(line.as_str());
        if line.trim().is_empty() {
            continue;
        }
        cleaned_lines.push(line);
    }

    cleaned_lines = dedupe_reply_lines(cleaned_lines);

    while cleaned_lines.first().is_some_and(String::is_empty) {
        cleaned_lines.remove(0);
    }
    while cleaned_lines.last().is_some_and(String::is_empty) {
        let _ = cleaned_lines.pop();
    }

    let joined = cleaned_lines.join("\n");
    ThreadLease::trim_wrapping_reply_artifacts(joined.as_str())
}

fn strip_reply_tag_fragments(raw_line: &str) -> String {
    raw_line
        .replace("<reply>", "")
        .replace("</reply>", "")
        .replace("<reply", "")
        .replace("</reply", "")
        .trim()
        .to_string()
}

fn dedupe_reply_lines(lines: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::<String>::new();
    for line in lines {
        let current = line.trim().to_string();
        if current.is_empty() {
            if deduped.last().is_some_and(|previous| previous.is_empty()) {
                continue;
            }
            deduped.push(String::new());
            continue;
        }

        if let Some(previous) = deduped.last() {
            if previous == &current {
                continue;
            }
            if should_collapse_prefix_lines(previous.as_str(), current.as_str()) {
                let _ = deduped.pop();
                deduped.push(current);
                continue;
            }
            if should_collapse_prefix_lines(current.as_str(), previous.as_str()) {
                continue;
            }
        }

        deduped.push(current);
    }
    deduped
}

fn should_collapse_prefix_lines(shorter: &str, longer: &str) -> bool {
    let shorter_trimmed = shorter.trim();
    let longer_trimmed = longer.trim();
    if shorter_trimmed.is_empty() || longer_trimmed.is_empty() {
        return false;
    }
    let shorter_normalized = normalize_for_prefix_comparison(shorter_trimmed);
    let longer_normalized = normalize_for_prefix_comparison(longer_trimmed);
    if shorter_normalized.is_empty() || longer_normalized.is_empty() {
        return false;
    }
    if longer_normalized.len() <= shorter_normalized.len() {
        return false;
    }
    if !longer_normalized.starts_with(shorter_normalized.as_str()) {
        return false;
    }
    let remainder = longer_normalized[shorter_normalized.len()..].trim_start();
    !remainder.is_empty()
        && (is_likely_stream_prefix_fragment(shorter_trimmed)
            || shorter_trimmed.split_whitespace().count() <= 12)
}

fn normalize_for_prefix_comparison(text: &str) -> String {
    text.replace('`', "").replace('"', "").trim().to_string()
}

fn is_likely_stream_prefix_fragment(text: &str) -> bool {
    if text.ends_with("...") {
        return true;
    }
    matches!(
        text.chars().last(),
        Some(',') | Some(':') | Some(';') | Some('-') | Some('(') | Some('`')
    ) || text.ends_with("&'")
        || text.ends_with("</reply")
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let chars = input.chars().collect::<Vec<_>>();
    let mut index = 0_usize;

    while index < chars.len() {
        let current = chars[index];
        if current == '\u{1b}' {
            index += 1;
            if index >= chars.len() {
                break;
            }
            match chars[index] {
                '[' => {
                    index += 1;
                    while index < chars.len() {
                        let ch = chars[index];
                        index += 1;
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                ']' => {
                    index += 1;
                    while index < chars.len() {
                        let ch = chars[index];
                        if ch == '\u{07}' {
                            index += 1;
                            break;
                        }
                        if ch == '\u{1b}' && chars.get(index + 1).copied() == Some('\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => {
                    index += 1;
                }
            }
            continue;
        }

        if current.is_control() && current != '\n' && current != '\t' {
            index += 1;
            continue;
        }

        output.push(current);
        index += 1;
    }

    output
}

fn strip_bracket_ansi_tokens(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let chars = line.chars().collect::<Vec<_>>();
    let mut index = 0_usize;

    while index < chars.len() {
        if chars[index] != '[' {
            output.push(chars[index]);
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == ';') {
            index += 1;
        }
        let is_ansi_token = index < chars.len()
            && matches!(
                chars[index],
                'm' | 'K' | 'A' | 'B' | 'C' | 'D' | 'G' | 'H' | 'J' | 'f'
            )
            && chars[(start + 1)..index]
                .iter()
                .any(|ch| ch.is_ascii_digit());
        if is_ansi_token {
            index += 1;
            continue;
        }

        output.push('[');
        index = start + 1;
    }

    output
}

fn is_terminal_chrome_line(line: &str) -> bool {
    line.starts_with("┌")
        || line.starts_with("└")
        || line.starts_with("│")
        || line.starts_with("▶")
        || line.starts_with("⬡")
        || line.starts_with("⬢")
        || line.ends_with('│')
        || line.contains("Add a follow-up")
        || line.contains("ctrl+c to stop")
        || line.contains("Auto-run all commands")
        || line.contains("Codex 5.3")
        || line.contains("Cursor Agent v")
        || line.starts_with("~/")
        || line.starts_with("Address Git PR comment ")
        || line == "Thread details:"
        || line.starts_with("GitLab discussion ")
        || line.starts_with("GitHub comment ")
        || (line.starts_with("note ") && line.contains("(resolvable="))
        || line.contains("include only the reviewer-facing")
        || line.contains("/ commands · @ files · ! shell")
        || line.contains("Generating..")
        || line.contains("tokens")
        || line.contains("[2K")
        || line.contains("[1A")
}

pub(crate) fn scope_terminal_output_to_prompt<'a>(
    terminal_output: &'a str,
    prompt_prefix: &str,
) -> &'a str {
    let Some(index) = terminal_output.rfind(prompt_prefix) else {
        return terminal_output;
    };
    let after_prompt_start = index + prompt_prefix.len();
    if after_prompt_start < terminal_output.len() {
        &terminal_output[after_prompt_start..]
    } else {
        ""
    }
}

pub(crate) fn with_git_reply_preamble(reply_body: &str) -> String {
    if reply_body.trim_start().starts_with(GIT_REPLY_PREAMBLE) {
        return reply_body.to_string();
    }
    format!("{GIT_REPLY_PREAMBLE}{reply_body}")
}
