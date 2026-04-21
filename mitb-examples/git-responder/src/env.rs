use core::time::Duration;
use std::collections::BTreeSet;

use crate::{GitProvider, ParsedPrUrl};

pub(crate) fn env_duration_seconds(env_key: &str, default_duration: Duration) -> Duration {
    let Some(raw_value) = optional_env_any(&[env_key]) else {
        return default_duration;
    };
    match raw_value.trim().parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => default_duration,
    }
}

pub(crate) fn env_flag(env_key: &str) -> bool {
    let Some(raw_value) = optional_env_any(&[env_key]) else {
        return false;
    };
    parse_bool_env_value(raw_value.as_str())
}

pub(crate) fn parse_bool_env_value(raw_value: &str) -> bool {
    matches!(
        raw_value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y"
    )
}

pub(crate) fn parse_allowed_users() -> Option<BTreeSet<String>> {
    let Some(raw_value) = optional_env_any(&[crate::ENV_GIT_ALLOWED_USERS]) else {
        return None;
    };
    let allowed: BTreeSet<String> = raw_value
        .split(',')
        .filter_map(|u| {
            let trimmed = u.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();
    if allowed.is_empty() {
        None
    } else {
        Some(allowed)
    }
}

pub(crate) fn parse_pr_url(raw: &str) -> Result<ParsedPrUrl, String> {
    let trimmed = raw.trim();
    let (scheme, remainder) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("MITB_GIT_PR_URL must be an absolute URL: `{trimmed}`"))?;
    if scheme.is_empty() {
        return Err(format!("MITB_GIT_PR_URL is missing scheme: `{trimmed}`"));
    }

    let slash_index = remainder
        .find('/')
        .ok_or_else(|| format!("MITB_GIT_PR_URL is missing path segments: `{trimmed}`"))?;
    let authority = &remainder[..slash_index];
    if authority.is_empty() {
        return Err(format!("MITB_GIT_PR_URL is missing authority: `{trimmed}`"));
    }
    let path_with_suffix = &remainder[slash_index..];
    let path_without_query = path_with_suffix
        .split('#')
        .next()
        .unwrap_or(path_with_suffix)
        .split('?')
        .next()
        .unwrap_or(path_with_suffix);
    let path_segments = path_without_query
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.trim().is_empty())
        .collect::<Vec<_>>();
    if path_segments.is_empty() {
        return Err(format!("MITB_GIT_PR_URL path was empty: `{trimmed}`"));
    }

    if let Some((project_path, pr_iid)) = parse_gitlab_path_segments(path_segments.as_slice()) {
        return Ok(ParsedPrUrl {
            provider: GitProvider::Gitlab,
            base_url: format!("{scheme}://{authority}"),
            project_path,
            pr_iid,
        });
    }

    if is_github_authority(authority) {
        if let Some((owner, repo, pr_iid)) =
            parse_pull_request_path_segments(path_segments.as_slice())
        {
            return Ok(ParsedPrUrl {
                provider: GitProvider::Github {
                    owner: owner.clone(),
                    repo: repo.clone(),
                },
                base_url: github_api_base_url(scheme),
                project_path: format!("{owner}/{repo}"),
                pr_iid,
            });
        }
    }

    Err(format!(
        "MITB_GIT_PR_URL must look like .../-/merge_requests/<id>, .../~/merge_requests/<id>, or https://github.com/<owner>/<repo>/pull/<id>: `{trimmed}`"
    ))
}

fn parse_gitlab_path_segments(segments: &[&str]) -> Option<(String, String)> {
    let marker_index = segments.windows(2).position(|window| {
        matches!(window.first().copied(), Some("-") | Some("~"))
            && window.get(1).copied() == Some("merge_requests")
    })?;
    if marker_index == 0 {
        return None;
    }
    let iid = segments.get(marker_index + 2)?.trim();
    if iid.is_empty() || !iid.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let mut decoded_segments = Vec::with_capacity(marker_index);
    for segment in &segments[..marker_index] {
        decoded_segments.push(decode_percent_escapes(segment)?);
    }
    let project_path = decoded_segments.join("/");
    if project_path.is_empty() {
        return None;
    }
    Some((project_path, iid.to_string()))
}

fn decode_percent_escapes(raw: &str) -> Option<String> {
    let mut output = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(char::from(bytes[index]));
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return None;
        }
        let high = hex_value(bytes[index + 1])?;
        let low = hex_value(bytes[index + 2])?;
        output.push(char::from((high << 4) | low));
        index += 3;
    }
    Some(output)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_pull_request_path_segments(segments: &[&str]) -> Option<(String, String, String)> {
    if segments.len() < 4 {
        return None;
    }
    if !matches!(segments.get(2).copied(), Some("pulls") | Some("pull")) {
        return None;
    }
    let owner = segments.first()?.trim();
    let repo = segments.get(1)?.trim();
    let iid = segments.get(3)?.trim();
    if owner.is_empty()
        || repo.is_empty()
        || iid.is_empty()
        || !iid.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some((owner.to_string(), repo.to_string(), iid.to_string()))
}

fn authority_host(authority: &str) -> &str {
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    host_port.split(':').next().unwrap_or(host_port)
}

fn is_github_authority(authority: &str) -> bool {
    matches!(
        authority_host(authority)
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "github.com" | "www.github.com" | "api.github.com"
    )
}

fn github_api_base_url(scheme: &str) -> String {
    format!("{scheme}://api.github.com")
}

pub(crate) fn required_env_any(names: &[&str]) -> Result<String, String> {
    optional_env_any(names).ok_or_else(|| {
        format!(
            "missing required environment variable; expected one of: {}",
            names.join(", ")
        )
    })
}

pub(crate) fn optional_env_any(names: &[&str]) -> Option<String> {
    let environment = crate::bindings::wasi::cli::environment::get_environment();
    for name in names {
        if let Some(value) = environment
            .iter()
            .find(|(key, value)| key == *name && !value.trim().is_empty())
            .map(|(_, value)| value.clone())
        {
            return Some(value);
        }
    }
    None
}
