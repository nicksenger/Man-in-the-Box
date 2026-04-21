use serde_json::Value;

use crate::{
    GitConfig, MAX_SNIPPET_BYTES, NotePosition, ParsedDiscussion, ParsedGithubReviewComment,
    ParsedNote, SNIPPET_CONTEXT_LINES,
};

pub(crate) fn author_in_allowlist(config: &GitConfig, author: &str) -> bool {
    config
        .allowed_users
        .as_ref()
        .map(|set| set.contains(author))
        .unwrap_or(true)
}

pub(crate) fn starts_with_mitb_prefix(body: &str) -> bool {
    const MITB_PREFIXES: [&str; 4] = [
        "**_Man in the Box_**, ",
        "Man in the Box, ",
        "Mitb, ",
        "mitb, ",
    ];
    MITB_PREFIXES.iter().any(|prefix| body.starts_with(prefix))
}

pub(crate) fn parse_github_review_comment(comment: &Value) -> Option<ParsedGithubReviewComment> {
    let id = comment
        .get("id")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if id == 0 {
        return None;
    }
    let body = comment
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if body.is_empty() {
        return None;
    }
    let author = comment
        .get("user")
        .and_then(|value| value.get("login").or_else(|| value.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let note = ParsedNote {
        id,
        author,
        body,
        resolvable: true,
        resolved: false,
        position: parse_github_note_position(comment),
    };

    Some(ParsedGithubReviewComment {
        note,
        in_reply_to_id: integer_field(comment, "in_reply_to_id"),
    })
}

fn parse_github_note_position(comment: &Value) -> Option<NotePosition> {
    let path = string_field(comment, "path");
    let new_line = integer_field(comment, "line").or_else(|| integer_field(comment, "position"));
    let old_line = integer_field(comment, "original_line")
        .or_else(|| integer_field(comment, "original_position"));
    let start_line = integer_field(comment, "start_line");
    let end_line = new_line.or(start_line);

    if path.is_none()
        && new_line.is_none()
        && old_line.is_none()
        && start_line.is_none()
        && end_line.is_none()
    {
        return None;
    }

    Some(NotePosition {
        path,
        new_line,
        old_line,
        start_line,
        end_line,
        line_code: None,
        position_type: Some(String::from("text")),
    })
}

pub(crate) fn parse_unresolved_discussion(discussion: &Value) -> Option<ParsedDiscussion> {
    let discussion_id = discussion.get("id")?.as_str()?.to_string();
    let notes = parse_discussion_notes(discussion.get("notes"))?;

    let has_unresolved_note = notes.iter().any(|note| note.resolvable && !note.resolved);
    let has_resolvable_note = notes.iter().any(|note| note.resolvable);
    let discussion_resolvable = discussion
        .get("resolvable")
        .and_then(Value::as_bool)
        .unwrap_or(has_resolvable_note);
    let discussion_resolved = discussion
        .get("resolved")
        .and_then(Value::as_bool)
        .unwrap_or(!has_unresolved_note);

    if !(discussion_resolvable && !discussion_resolved) {
        return None;
    }

    let comment_id = notes
        .iter()
        .find(|note| note.resolvable && !note.resolved)
        .map(|note| note.id)
        .or_else(|| notes.first().map(|note| note.id))
        .unwrap_or_default();
    Some(ParsedDiscussion {
        discussion_id,
        comment_id,
        notes,
    })
}

pub(crate) fn parse_discussion_notes(notes_value: Option<&Value>) -> Option<Vec<ParsedNote>> {
    let notes_array = notes_value?.as_array()?;
    let mut notes = Vec::<ParsedNote>::new();
    for note in notes_array {
        let id = note.get("id").and_then(Value::as_u64).unwrap_or_default();
        if id == 0 {
            continue;
        }

        let body = note
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if body.is_empty() {
            continue;
        }

        let author = note
            .get("author")
            .and_then(|value| value.get("username").or_else(|| value.get("name")))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let resolvable = note
            .get("resolvable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resolved = note
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let position = parse_note_position(note);
        notes.push(ParsedNote {
            id,
            author,
            body,
            resolvable,
            resolved,
            position,
        });
    }

    if notes.is_empty() { None } else { Some(notes) }
}

pub(crate) async fn format_discussion_summary(discussion_id: &str, notes: &[ParsedNote]) -> String {
    let mut summary = format!("GitLab discussion {discussion_id}\n");
    for note in notes {
        summary.push_str(
            format!("- note {} by @{}:\n{}\n\n", note.id, note.author, note.body).as_str(),
        );
        if let Some(position) = note.position.as_ref() {
            summary.push_str(
                format!("  location: {}\n", describe_position(position).as_str()).as_str(),
            );
            if let Some(snippet) = snippet_for_position(position).await {
                summary.push_str("  code snippet:\n");
                summary.push_str(snippet.as_str());
                summary.push('\n');
            }
        }
        summary.push('\n');
    }
    summary
}

pub(crate) async fn format_github_comment_summary(comment_id: u64, note: &ParsedNote) -> String {
    let mut summary = format!("GitHub comment {comment_id}\n");
    summary
        .push_str(format!("- note {} by @{}:\n{}\n\n", note.id, note.author, note.body).as_str());
    if let Some(position) = note.position.as_ref() {
        summary.push_str(format!("  location: {}\n", describe_position(position)).as_str());
        if let Some(snippet) = snippet_for_position(position).await {
            summary.push_str("  code snippet:\n");
            summary.push_str(snippet.as_str());
            summary.push('\n');
        }
    }
    summary.push('\n');
    summary
}

fn parse_note_position(note: &Value) -> Option<NotePosition> {
    let position = note.get("position")?;
    if position.is_null() {
        return None;
    }

    let path = string_field(position, "new_path")
        .or_else(|| string_field(position, "old_path"))
        .or_else(|| string_field(note, "path"));
    let new_line = integer_field(position, "new_line");
    let old_line = integer_field(position, "old_line");
    let line_code = string_field(position, "line_code").or_else(|| string_field(note, "line_code"));
    let position_type = string_field(position, "position_type");
    let (start_line, end_line) = parse_line_range(position.get("line_range"));

    if path.is_none()
        && new_line.is_none()
        && old_line.is_none()
        && start_line.is_none()
        && end_line.is_none()
        && line_code.is_none()
        && position_type.is_none()
    {
        return None;
    }

    Some(NotePosition {
        path,
        new_line,
        old_line,
        start_line,
        end_line,
        line_code,
        position_type,
    })
}

pub(crate) fn max_note_id(notes: &[ParsedNote]) -> u64 {
    notes.iter().map(|note| note.id).max().unwrap_or_default()
}

fn parse_line_range(line_range: Option<&Value>) -> (Option<u64>, Option<u64>) {
    let Some(range) = line_range else {
        return (None, None);
    };
    let Some(start) = range.get("start") else {
        return (None, None);
    };
    let Some(end) = range.get("end") else {
        return (None, None);
    };

    let start_line = integer_field(start, "new_line").or_else(|| integer_field(start, "old_line"));
    let end_line = integer_field(end, "new_line").or_else(|| integer_field(end, "old_line"));
    (start_line, end_line)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn integer_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

pub(crate) fn describe_position(position: &NotePosition) -> String {
    let mut parts = Vec::<String>::new();
    if let Some(path) = position.path.as_ref() {
        parts.push(format!("path={path}"));
    }
    if let Some(new_line) = position.new_line {
        parts.push(format!("new_line={new_line}"));
    }
    if let Some(old_line) = position.old_line {
        parts.push(format!("old_line={old_line}"));
    }
    if let (Some(start), Some(end)) = (position.start_line, position.end_line) {
        parts.push(format!("line_range={start}-{end}"));
    }
    if let Some(position_type) = position.position_type.as_ref() {
        parts.push(format!("position_type={position_type}"));
    }
    if let Some(line_code) = position.line_code.as_ref() {
        parts.push(format!("line_code={line_code}"));
    }

    if parts.is_empty() {
        String::from("unavailable")
    } else {
        parts.join(", ")
    }
}

async fn snippet_for_position(position: &NotePosition) -> Option<String> {
    let path = position.path.as_ref()?;
    let text = mitb_sdk::fs::read_text(path.as_str()).await.ok()?;
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }

    let total_lines = lines.len();
    let (base_start, base_end) = position_line_bounds(position)?;
    let bounded_start = base_start.max(1).min(total_lines);
    let bounded_end = base_end.max(bounded_start).min(total_lines);

    let window_start = bounded_start.saturating_sub(SNIPPET_CONTEXT_LINES).max(1);
    let window_end = bounded_end
        .saturating_add(SNIPPET_CONTEXT_LINES)
        .min(total_lines);

    let mut snippet = String::new();
    snippet.push_str(format!("```{}\n", path).as_str());
    for line_number in window_start..=window_end {
        let text = lines
            .get(line_number.saturating_sub(1))
            .copied()
            .unwrap_or("");
        let marker = if line_number >= bounded_start && line_number <= bounded_end {
            ">"
        } else {
            " "
        };
        snippet.push_str(format!("{marker} {line_number:>6} | {text}\n").as_str());
    }
    snippet.push_str("```");
    Some(mitb_sdk::truncate(snippet.as_str(), MAX_SNIPPET_BYTES))
}

fn position_line_bounds(position: &NotePosition) -> Option<(usize, usize)> {
    if let (Some(start), Some(end)) = (position.start_line, position.end_line) {
        return Some((start as usize, end as usize));
    }

    if let Some(new_line) = position.new_line {
        return Some((new_line as usize, new_line as usize));
    }

    if let Some(old_line) = position.old_line {
        return Some((old_line as usize, old_line as usize));
    }

    None
}
