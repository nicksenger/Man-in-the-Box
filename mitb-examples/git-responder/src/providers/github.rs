use std::collections::BTreeMap;

use crate::{
    DEFAULT_PAGE_SIZE, GitConfig, GitProvider, MAX_THREAD_SUMMARY_BYTES, ParsedGithubReviewComment,
    ParsedNote, UnresolvedThread,
};

pub(crate) async fn list_unresolved_threads(
    config: &GitConfig,
) -> Result<Vec<UnresolvedThread>, String> {
    let mut page = 1_usize;
    let mut comments = Vec::<ParsedGithubReviewComment>::new();

    loop {
        let url = review_comments_list_url(config, page, DEFAULT_PAGE_SIZE)?;
        let payload =
            crate::git_request_json(mitb_sdk::http::HttpMethod::Get, url.as_str(), config, None)
                .await?;
        let Some(page_items) = payload.as_array() else {
            return Err(format!(
                "GitHub review comments response was not an array: {}",
                mitb_sdk::truncate(payload.to_string().as_str(), 1024)
            ));
        };

        for comment in page_items {
            if let Some(parsed) = crate::parse_github_review_comment(comment) {
                comments.push(parsed);
            }
        }

        if page_items.len() < DEFAULT_PAGE_SIZE {
            break;
        }
        page = page.saturating_add(1);
    }

    comments.sort_by_key(|comment| comment.note.id);
    let mut replies_by_parent = BTreeMap::<u64, Vec<u64>>::new();
    for comment in comments.iter() {
        if let Some(parent_id) = comment.in_reply_to_id {
            replies_by_parent
                .entry(parent_id)
                .or_default()
                .push(comment.note.id);
        }
    }

    let mut unresolved = Vec::<UnresolvedThread>::new();
    for comment in comments {
        if !crate::starts_with_mitb_prefix(comment.note.body.as_str()) {
            continue;
        }
        if !crate::author_in_allowlist(config, comment.note.author.as_str()) {
            continue;
        }
        if replies_by_parent
            .get(&comment.note.id)
            .is_some_and(|replies| !replies.is_empty())
        {
            continue;
        }
        let summary = mitb_sdk::truncate(
            crate::format_github_comment_summary(comment.note.id, &comment.note)
                .await
                .as_str(),
            MAX_THREAD_SUMMARY_BYTES,
        );
        unresolved.push(UnresolvedThread {
            discussion_id: comment.note.id.to_string(),
            comment_id: comment.note.id,
            reaction_comment_id: comment.note.id,
            summary,
            last_seen_note_id: comment.note.id,
        });
    }

    Ok(unresolved)
}

pub(crate) fn review_comment_reply_url(
    config: &GitConfig,
    comment_id: u64,
) -> Result<String, String> {
    let (owner, repo) = owner_repo(config)?;
    Ok(format!(
        "{}/repos/{}/{}/pulls/{}/comments/{}/replies",
        config.base_url,
        crate::url_encode_component(owner),
        crate::url_encode_component(repo),
        crate::url_encode_component(config.pr_iid.as_str()),
        comment_id,
    ))
}

pub(crate) fn review_comment_reaction_url(
    config: &GitConfig,
    comment_id: u64,
) -> Result<String, String> {
    let (owner, repo) = owner_repo(config)?;
    Ok(format!(
        "{}/repos/{}/{}/pulls/comments/{}/reactions",
        config.base_url,
        crate::url_encode_component(owner),
        crate::url_encode_component(repo),
        comment_id,
    ))
}

pub(crate) async fn fetch_new_thread_replies(
    config: &GitConfig,
    discussion_id: &str,
    since_note_id: u64,
) -> Result<(Option<String>, u64), String> {
    let tracked_comment_id = discussion_id
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid GitHub comment id `{discussion_id}`"))?;

    let mut page = 1_usize;
    let mut latest_note_id = since_note_id;
    let mut new_notes = Vec::<ParsedNote>::new();

    loop {
        let url = review_comments_list_url(config, page, DEFAULT_PAGE_SIZE)?;
        let payload =
            crate::git_request_json(mitb_sdk::http::HttpMethod::Get, url.as_str(), config, None)
                .await?;
        let Some(page_items) = payload.as_array() else {
            return Err(format!(
                "GitHub review comments response was not an array: {}",
                mitb_sdk::truncate(payload.to_string().as_str(), 1024)
            ));
        };

        for item in page_items {
            if let Some(comment) = crate::parse_github_review_comment(item) {
                latest_note_id = latest_note_id.max(comment.note.id);
                if comment.note.id <= since_note_id {
                    continue;
                }
                if comment.note.id == tracked_comment_id
                    || comment.in_reply_to_id == Some(tracked_comment_id)
                {
                    new_notes.push(comment.note);
                }
            }
        }
        if page_items.len() < DEFAULT_PAGE_SIZE {
            break;
        }
        page = page.saturating_add(1);
    }

    if new_notes.is_empty() {
        return Ok((None, latest_note_id));
    }

    new_notes.sort_by_key(|note| note.id);
    let mut text =
        String::from("New GitHub replies on this thread since the last verification attempt:\n");
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

fn review_comments_list_url(
    config: &GitConfig,
    page: usize,
    per_page: usize,
) -> Result<String, String> {
    let (owner, repo) = owner_repo(config)?;
    Ok(format!(
        "{}/repos/{}/{}/pulls/{}/comments?per_page={}&page={}",
        config.base_url,
        crate::url_encode_component(owner),
        crate::url_encode_component(repo),
        crate::url_encode_component(config.pr_iid.as_str()),
        per_page,
        page
    ))
}

fn owner_repo(config: &GitConfig) -> Result<(&str, &str), String> {
    match &config.provider {
        GitProvider::Github { owner, repo } => Ok((owner.as_str(), repo.as_str())),
        GitProvider::Gitlab => Err(String::from(
            "github owner/repo requested for non-github provider",
        )),
    }
}
