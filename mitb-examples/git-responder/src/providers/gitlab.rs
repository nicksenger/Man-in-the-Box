use crate::{DEFAULT_PAGE_SIZE, GitConfig, MAX_THREAD_SUMMARY_BYTES, UnresolvedThread};

pub(crate) async fn list_unresolved_threads(
    config: &GitConfig,
) -> Result<Vec<UnresolvedThread>, String> {
    let mut page = 1_usize;
    let mut unresolved = Vec::<UnresolvedThread>::new();

    loop {
        let url = discussions_list_url(config, page, DEFAULT_PAGE_SIZE);
        let payload =
            crate::git_request_json(mitb_sdk::http::HttpMethod::Get, url.as_str(), config, None)
                .await?;
        let Some(page_items) = payload.as_array() else {
            return Err(format!(
                "GitLab discussions response was not an array: {}",
                mitb_sdk::truncate(payload.to_string().as_str(), 1024)
            ));
        };

        for discussion in page_items {
            if let Some(parsed_discussion) = crate::parse_unresolved_discussion(discussion) {
                let summary = mitb_sdk::truncate(
                    crate::format_discussion_summary(
                        parsed_discussion.discussion_id.as_str(),
                        &parsed_discussion.notes,
                    )
                    .await
                    .as_str(),
                    MAX_THREAD_SUMMARY_BYTES,
                );
                // Check the absolute last note first: if mitb's response is the
                // terminal note, the thread is resolved regardless of who commented
                // most recently. Only fall back to filtered_notes when the terminal
                // note is from a non-allowed user.
                if let Some(note) = parsed_discussion.notes.last() {
                    if crate::starts_with_mitb_prefix(note.body.as_str())
                        && crate::author_in_allowlist(config, note.author.as_str())
                    {
                        unresolved.push(UnresolvedThread {
                            discussion_id: parsed_discussion.discussion_id,
                            comment_id: parsed_discussion.comment_id,
                            reaction_comment_id: note.id,
                            summary,
                            last_seen_note_id: crate::max_note_id(
                                parsed_discussion.notes.as_slice(),
                            ),
                        });
                    }
                }
            }
        }

        if page_items.len() < DEFAULT_PAGE_SIZE {
            break;
        }
        page = page.saturating_add(1);
    }

    Ok(unresolved)
}

pub(crate) fn discussion_reply_url(config: &GitConfig, discussion_id: &str) -> String {
    format!(
        "{}/api/v4/projects/{}/merge_requests/{}/discussions/{}/notes",
        config.base_url,
        crate::url_encode_component(config.gitlab_project_path_segment()),
        crate::url_encode_component(config.pr_iid.as_str()),
        crate::url_encode_component(discussion_id),
    )
}

pub(crate) fn discussion_resolve_url(config: &GitConfig, discussion_id: &str) -> String {
    format!(
        "{}/api/v4/projects/{}/merge_requests/{}/discussions/{}",
        config.base_url,
        crate::url_encode_component(config.gitlab_project_path_segment()),
        crate::url_encode_component(config.pr_iid.as_str()),
        crate::url_encode_component(discussion_id),
    )
}

pub(crate) fn merge_request_note_reaction_url(
    config: &GitConfig,
    note_id: u64,
    reaction: &str,
) -> String {
    format!(
        "{}/api/v4/projects/{}/merge_requests/{}/notes/{}/award_emoji?name={}",
        config.base_url,
        crate::url_encode_component(config.gitlab_project_path_segment()),
        crate::url_encode_component(config.pr_iid.as_str()),
        note_id,
        crate::url_encode_component(reaction),
    )
}

pub(crate) async fn fetch_new_thread_replies(
    config: &GitConfig,
    discussion_id: &str,
    since_note_id: u64,
) -> Result<(Option<String>, u64), String> {
    let payload = crate::git_request_json(
        mitb_sdk::http::HttpMethod::Get,
        discussion_get_url(config, discussion_id).as_str(),
        config,
        None,
    )
    .await?;
    let notes = crate::parse_discussion_notes(payload.get("notes")).unwrap_or_default();
    let latest_note_id = crate::max_note_id(notes.as_slice()).max(since_note_id);
    let new_notes = notes
        .iter()
        .filter(|note| note.id > since_note_id)
        .collect::<Vec<_>>();
    if new_notes.is_empty() {
        return Ok((None, latest_note_id));
    }

    let mut text =
        String::from("New GitLab replies on this thread since the last verification attempt:\n");
    for note in new_notes {
        text.push_str(
            format!("- note {} by @{}:\n{}\n\n", note.id, note.author, note.body).as_str(),
        );
    }
    Ok((
        Some(mitb_sdk::truncate(
            text.trim_end(),
            MAX_THREAD_SUMMARY_BYTES,
        )),
        latest_note_id,
    ))
}

fn discussions_list_url(config: &GitConfig, page: usize, per_page: usize) -> String {
    format!(
        "{}/api/v4/projects/{}/merge_requests/{}/discussions?per_page={}&page={}",
        config.base_url,
        crate::url_encode_component(config.gitlab_project_path_segment()),
        crate::url_encode_component(config.pr_iid.as_str()),
        per_page,
        page
    )
}

fn discussion_get_url(config: &GitConfig, discussion_id: &str) -> String {
    format!(
        "{}/api/v4/projects/{}/merge_requests/{}/discussions/{}",
        config.base_url,
        crate::url_encode_component(config.gitlab_project_path_segment()),
        crate::url_encode_component(config.pr_iid.as_str()),
        crate::url_encode_component(discussion_id),
    )
}

#[cfg(test)]
mod tests {
    use super::merge_request_note_reaction_url;
    use crate::{GitConfig, GitProvider, IntegrationMode, ResponderMode};
    use core::time::Duration;

    #[test]
    fn merge_request_note_reaction_url_targets_merge_request_note_award_endpoint() {
        let config = GitConfig {
            provider: GitProvider::Gitlab,
            base_url: String::from("https://gitlab.example.com"),
            token: String::from("token"),
            project: String::from("group/project"),
            project_api_id: None,
            pr_iid: String::from("12"),
            verification_command: None,
            verification_timeout: Duration::from_secs(480),
            responder_mode: ResponderMode::ReadWrite,
            clear_cmd: None,
            drip: None,
            allowed_users: None,
            lease: None,
            integration_mode: IntegrationMode::DirectPush,
        };

        let url = merge_request_note_reaction_url(&config, 593, "ballot_box_with_check");
        assert_eq!(
            url,
            "https://gitlab.example.com/api/v4/projects/group%2Fproject/merge_requests/12/notes/593/award_emoji?name=ballot_box_with_check"
        );
    }
}
