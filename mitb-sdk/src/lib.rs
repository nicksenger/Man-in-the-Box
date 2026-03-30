//! Utilities for MITB policy guest modules.

use core::future::Future;
use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration;
use std::path::PathBuf;

pub use futures;
pub mod fs;
pub mod git;
pub mod search;

pub const MITB_HOME_DIR_ENV: &str = "MITB_HOME_DIR";
pub const MITB_ALIAS_ENV: &str = "MITB_ALIAS";

pub mod http {
    use core::time::Duration;
    use wasip3::http::client;
    use wasip3::http::types::{
        ErrorCode, Fields, HeaderError, Method, Request, RequestOptions, RequestOptionsError,
        Response, Scheme,
    };
    use wasip3::{wit_future, wit_stream};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HttpMethod {
        Get,
        Head,
        Post,
        Put,
        Delete,
        Connect,
        Options,
        Trace,
        Patch,
        Other(String),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct HttpRequest {
        pub method: HttpMethod,
        pub url: String,
        pub headers: Vec<(String, Vec<u8>)>,
        pub body: Vec<u8>,
        pub connect_timeout: Option<Duration>,
        pub first_byte_timeout: Option<Duration>,
        pub between_bytes_timeout: Option<Duration>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct HttpResponse {
        pub status: u16,
        pub headers: Vec<(String, Vec<u8>)>,
        pub body: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    struct ParsedUrl {
        scheme: Scheme,
        authority: String,
        path_with_query: String,
    }

    impl HttpRequest {
        pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
            Self {
                method,
                url: url.into(),
                headers: Vec::new(),
                body: Vec::new(),
                connect_timeout: None,
                first_byte_timeout: None,
                between_bytes_timeout: None,
            }
        }

        pub fn get(url: impl Into<String>) -> Self {
            Self::new(HttpMethod::Get, url)
        }

        pub fn post(url: impl Into<String>) -> Self {
            Self::new(HttpMethod::Post, url)
        }

        pub fn header(mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }

        pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
            self.body = body.into();
            self
        }

        pub fn connect_timeout(mut self, duration: Duration) -> Self {
            self.connect_timeout = Some(duration);
            self
        }

        pub fn first_byte_timeout(mut self, duration: Duration) -> Self {
            self.first_byte_timeout = Some(duration);
            self
        }

        pub fn between_bytes_timeout(mut self, duration: Duration) -> Self {
            self.between_bytes_timeout = Some(duration);
            self
        }
    }

    impl HttpResponse {
        pub fn text(&self) -> Result<String, String> {
            String::from_utf8(self.body.clone())
                .map_err(|error| format!("response body was not valid utf-8: {error}"))
        }

        pub fn header(&self, name: &str) -> Option<&[u8]> {
            self.headers
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_slice())
        }
    }

    pub async fn send(request: HttpRequest) -> Result<HttpResponse, String> {
        let request_label = format_request_label(&request);
        let parsed_url = parse_url(&request.url)?;
        let headers = Fields::from_list(&request.headers).map_err(|error| {
            format!(
                "failed constructing HTTP request `{request_label}`: {}",
                format_header_error(error)
            )
        })?;
        let options = build_request_options(&request).map_err(|error| {
            format!("failed configuring HTTP request `{request_label}`: {error}")
        })?;

        let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
        let (body, write_body) = if request.body.is_empty() {
            drop(trailers_tx);
            (None, None)
        } else {
            let (mut body_tx, body_rx) = wit_stream::new::<u8>();
            let body = request.body.clone();
            let writer = async move {
                let remaining = body_tx.write_all(body).await;
                drop(body_tx);
                drop(trailers_tx);
                if remaining.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "request body stream stopped before all bytes were written ({} bytes remaining)",
                        remaining.len()
                    ))
                }
            };
            (Some(body_rx), Some(writer))
        };

        let (request_handle, request_result) = Request::new(headers, body, trailers_rx, options);
        request_handle
            .set_method(&into_wasi_method(&request.method))
            .map_err(|_| String::from("request method was rejected by wasi:http"))?;
        request_handle
            .set_scheme(Some(&parsed_url.scheme))
            .map_err(|_| String::from("request scheme was rejected by wasi:http"))?;
        request_handle
            .set_authority(Some(&parsed_url.authority))
            .map_err(|_| String::from("request authority was rejected by wasi:http"))?;
        request_handle
            .set_path_with_query(Some(&parsed_url.path_with_query))
            .map_err(|_| String::from("request path/query was rejected by wasi:http"))?;

        let response = if let Some(write_body) = write_body {
            let (write_result, response_result) =
                crate::futures::join!(write_body, async { client::send(request_handle).await });
            write_result?;
            response_result.map_err(|error| {
                format!(
                    "HTTP request `{request_label}` failed: {}",
                    format_http_error(error)
                )
            })?
        } else {
            client::send(request_handle).await.map_err(|error| {
                format!(
                    "HTTP request `{request_label}` failed: {}",
                    format_http_error(error)
                )
            })?
        };

        let headers = response.get_headers().copy_all();
        let status = response.get_status_code();
        let (response_result_tx, response_result_rx) = wit_future::new(|| Ok(()));
        let (body_stream, trailers_future) = Response::consume_body(response, response_result_rx);
        let body = body_stream.collect().await;
        drop(response_result_tx);
        let _ = trailers_future.into_future().await.map_err(|error| {
            format!(
                "HTTP response trailers for `{request_label}` failed: {}",
                format_http_error(error)
            )
        })?;
        request_result.into_future().await.map_err(|error| {
            format!(
                "HTTP request finalization for `{request_label}` failed: {}",
                format_http_error(error)
            )
        })?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn format_request_label(request: &HttpRequest) -> String {
        format!("{} {}", format_method(&request.method), request.url)
    }

    fn format_method(method: &HttpMethod) -> &str {
        match method {
            HttpMethod::Get => "GET",
            HttpMethod::Head => "HEAD",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Connect => "CONNECT",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Trace => "TRACE",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Other(other) => other.as_str(),
        }
    }

    fn build_request_options(request: &HttpRequest) -> Result<Option<RequestOptions>, String> {
        if request.connect_timeout.is_none()
            && request.first_byte_timeout.is_none()
            && request.between_bytes_timeout.is_none()
        {
            return Ok(None);
        }

        let options = RequestOptions::new();
        options
            .set_connect_timeout(request.connect_timeout.map(super::duration_to_nanos_u64))
            .map_err(format_request_options_error)?;
        options
            .set_first_byte_timeout(request.first_byte_timeout.map(super::duration_to_nanos_u64))
            .map_err(format_request_options_error)?;
        options
            .set_between_bytes_timeout(
                request
                    .between_bytes_timeout
                    .map(super::duration_to_nanos_u64),
            )
            .map_err(format_request_options_error)?;
        Ok(Some(options))
    }

    fn parse_url(url: &str) -> Result<ParsedUrl, String> {
        let (scheme_text, remainder) = url
            .split_once("://")
            .ok_or_else(|| format!("url `{url}` must be absolute and include a scheme"))?;
        let scheme = match scheme_text {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => Scheme::Other(other.to_string()),
        };

        if remainder.is_empty() {
            return Err(format!("url `{url}` is missing an authority"));
        }

        let split_index = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let authority = remainder[..split_index].to_string();
        if authority.is_empty() {
            return Err(format!("url `{url}` is missing an authority"));
        }

        let suffix = &remainder[split_index..];
        let path_with_query = if suffix.is_empty() {
            String::from("/")
        } else if suffix.starts_with('/') {
            suffix.to_string()
        } else if suffix.starts_with('?') {
            format!("/{suffix}")
        } else {
            return Err(format!("url `{url}` must not include a fragment"));
        };

        Ok(ParsedUrl {
            scheme,
            authority,
            path_with_query,
        })
    }

    fn into_wasi_method(method: &HttpMethod) -> Method {
        match method {
            HttpMethod::Get => Method::Get,
            HttpMethod::Head => Method::Head,
            HttpMethod::Post => Method::Post,
            HttpMethod::Put => Method::Put,
            HttpMethod::Delete => Method::Delete,
            HttpMethod::Connect => Method::Connect,
            HttpMethod::Options => Method::Options,
            HttpMethod::Trace => Method::Trace,
            HttpMethod::Patch => Method::Patch,
            HttpMethod::Other(other) => Method::Other(other.clone()),
        }
    }

    fn format_http_error(error: ErrorCode) -> String {
        format!("wasi:http request failed: {error:?}")
    }

    fn format_header_error(error: HeaderError) -> String {
        format!("invalid HTTP headers: {error:?}")
    }

    fn format_request_options_error(error: RequestOptionsError) -> String {
        format!("request options were rejected: {error:?}")
    }
}

/// Default transcript window requested by SDK helpers.
pub const DEFAULT_TERMINAL_MAX_BYTES: u32 = 512 * 1024;

/// Default timeout for [`policy_prelude!`] process helpers.
pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Stateful exponential backoff helper for retry loops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExponentialBackoff {
    initial_interval: Duration,
    current_interval: Duration,
    max_interval: Duration,
    multiplier: u32,
}

impl ExponentialBackoff {
    pub fn new(initial_interval: Duration, max_interval: Duration) -> Self {
        Self {
            initial_interval,
            current_interval: initial_interval,
            max_interval,
            multiplier: 2,
        }
    }

    pub fn with_multiplier(mut self, multiplier: u32) -> Self {
        self.multiplier = multiplier.max(1);
        self
    }

    pub fn next_backoff(&mut self) -> Duration {
        let backoff = self.current_interval.min(self.max_interval);
        let factor = u64::from(self.multiplier.max(1));
        self.current_interval = backoff
            .checked_mul(factor as u32)
            .unwrap_or(self.max_interval)
            .min(self.max_interval);
        backoff
    }

    pub fn reset(&mut self) {
        self.current_interval = self.initial_interval;
    }

    pub fn current_interval(&self) -> Duration {
        self.current_interval.min(self.max_interval)
    }
}

#[doc(hidden)]
pub fn duration_to_nanos_u64(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutOutcome<T> {
    Completed(T),
    TimedOut,
}

/// Race a future against a timeout future and drop the loser.
pub async fn with_timeout<F, T, TFut, U>(future: F, timeout: TFut) -> TimeoutOutcome<T>
where
    F: Future<Output = T>,
    TFut: Future<Output = U>,
{
    futures::pin_mut!(future);
    futures::pin_mut!(timeout);

    match futures::future::select(future, timeout).await {
        futures::future::Either::Left((value, _)) => TimeoutOutcome::Completed(value),
        futures::future::Either::Right((_, _)) => TimeoutOutcome::TimedOut,
    }
}

#[cfg(feature = "build-support")]
pub mod build_support;

#[derive(Clone, Debug, Default)]
pub struct IdleTracker {
    previous_contents: Option<String>,
    startup_grace_until_ns: Option<u64>,
}

/// Return the current user's home directory from the guest environment.
pub fn home_dir() -> Result<PathBuf, String> {
    home_dir_from_environment(&wasip3::cli::environment::get_environment())
}

/// Return the optional CLI alias from the guest environment.
pub fn alias() -> Option<String> {
    env_value(&wasip3::cli::environment::get_environment(), MITB_ALIAS_ENV).map(str::to_string)
}

fn home_dir_from_environment(environment: &[(String, String)]) -> Result<PathBuf, String> {
    if let Some(home) = env_value(environment, MITB_HOME_DIR_ENV) {
        return Ok(PathBuf::from(home));
    }

    if let Some(home) = env_value(environment, "HOME") {
        return Ok(PathBuf::from(home));
    }

    if let Some(user_profile) = env_value(environment, "USERPROFILE") {
        return Ok(PathBuf::from(user_profile));
    }

    let home_drive = env_value(environment, "HOMEDRIVE");
    let home_path = env_value(environment, "HOMEPATH");
    if let (Some(home_drive), Some(home_path)) = (home_drive, home_path) {
        return Ok(PathBuf::from(format!("{home_drive}{home_path}")));
    }

    Err(String::from(
        "could not determine home directory from MITB_HOME_DIR, HOME, USERPROFILE, or HOMEDRIVE/HOMEPATH",
    ))
}

fn env_value<'a>(environment: &'a [(String, String)], key: &str) -> Option<&'a str> {
    environment
        .iter()
        .find(|(candidate, value)| candidate == key && !value.is_empty())
        .map(|(_, value)| value.as_str())
}

/// Default idle detection for a single policy session.
///
/// Returns `true` when `contents` matches the previous value observed by the
/// same tracker and the initial startup grace period has elapsed, then stores
/// `contents` as the latest snapshot.
pub fn detect_idle(tracker: &mut IdleTracker, contents: &str) -> bool {
    detect_idle_at(
        tracker,
        contents,
        wasip3::clocks::monotonic_clock::now(),
        duration_to_nanos_u64(Duration::from_secs(5)),
    )
}

fn detect_idle_at(
    tracker: &mut IdleTracker,
    contents: &str,
    now_ns: u64,
    startup_grace_ns: u64,
) -> bool {
    let idle = tracker
        .previous_contents
        .as_ref()
        .map(|previous| previous == contents)
        .unwrap_or(false);
    tracker.previous_contents = Some(contents.to_string());

    if tracker.startup_grace_until_ns.is_none() {
        tracker.startup_grace_until_ns = Some(now_ns.saturating_add(startup_grace_ns));
    }

    if !idle {
        return false;
    }

    tracker
        .startup_grace_until_ns
        .map(|grace_until_ns| now_ns >= grace_until_ns)
        .unwrap_or(true)
}

/// Logging levels used by guest-side helper utilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

static MAX_LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// Set the maximum enabled log level for helpers created by [`policy_prelude!`].
pub fn set_max_log_level(level: LogLevel) {
    MAX_LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Return `true` when the given level should be emitted.
pub fn log_enabled(level: LogLevel) -> bool {
    (level as u8) <= MAX_LOG_LEVEL.load(Ordering::Relaxed)
}

/// Canonical, fixed-width string tag for a log level.
pub fn level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => " WARN",
        LogLevel::Info => " INFO",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    }
}

/// Parse a log level from a string.
///
/// Supports simple levels (for example `info`) and target-prefixed variants
/// (for example `some_target=debug`) by parsing the final `=` segment.
pub fn parse_log_level(value: &str) -> Option<LogLevel> {
    let level_part = if let Some((_, tail)) = value.rsplit_once('=') {
        tail
    } else {
        value
    };

    let normalized = level_part.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "error" => Some(LogLevel::Error),
        "warn" | "warning" => Some(LogLevel::Warn),
        "info" => Some(LogLevel::Info),
        "debug" => Some(LogLevel::Debug),
        "trace" => Some(LogLevel::Trace),
        _ => None,
    }
}

/// Deterministic Fisher-Yates shuffle using a simple LCG seeded by `seed`.
pub fn pseudo_shuffle<T>(items: &mut [T], seed: u64) {
    let n = items.len();
    if n <= 1 {
        return;
    }

    let mut state = seed;
    for i in (1..n).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((state >> 33) as usize) % (i + 1);
        items.swap(i, j);
    }
}

/// Truncate UTF-8 text to at most `max_len` bytes and append `...` when
/// truncation occurs.
pub fn truncate(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }

    let mut end = max_len;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    let mut output = String::with_capacity(end + 3);
    output.push_str(&text[..end]);
    output.push_str("...");
    output
}

/// Return all regex capture group matches from `text` in match order.
///
/// `capture_index` is the capture group to return (`0` is the full match).
pub fn regex_captures(
    text: &str,
    pattern: &str,
    capture_index: usize,
) -> Result<Vec<String>, String> {
    let regex = regex::Regex::new(pattern)
        .map_err(|error| format!("invalid regex pattern `{pattern}`: {error}"))?;
    let mut captures_out = Vec::new();
    for captures in regex.captures_iter(text) {
        if let Some(matched) = captures.get(capture_index) {
            captures_out.push(matched.as_str().to_string());
        }
    }
    Ok(captures_out)
}

/// Return the first regex capture group match from `text`.
///
/// `capture_index` is the capture group to return (`0` is the full match).
pub fn regex_capture_first(
    text: &str,
    pattern: &str,
    capture_index: usize,
) -> Result<Option<String>, String> {
    let regex = regex::Regex::new(pattern)
        .map_err(|error| format!("invalid regex pattern `{pattern}`: {error}"))?;
    for captures in regex.captures_iter(text) {
        if let Some(matched) = captures.get(capture_index) {
            return Ok(Some(matched.as_str().to_string()));
        }
    }
    Ok(None)
}

/// Return the most recent regex capture group match from `text`.
///
/// `capture_index` is the capture group to return (`0` is the full match).
pub fn regex_capture(
    text: &str,
    pattern: &str,
    capture_index: usize,
) -> Result<Option<String>, String> {
    Ok(regex_captures(text, pattern, capture_index)?
        .into_iter()
        .last())
}

/// Generate common policy guest helper functions:
/// - `init_logging`
/// - `log_error`, `log_warn`, `log_info`, `log_debug`, `log_trace`
/// - `prompt!`, `report_reward!`, `regex_capture!`, `regex_capture_first!`,
///   `regex_captures!`
/// - `print_line`
/// - `terminal_head`, `terminal_snapshot`, `terminal_read_since`,
///   `terminal_snapshot_text`, `terminal_read_since_text`
/// - `run_process`, `run_process_with_timeout`, `report_reward`
///
/// `policy!(bindings, scope: "...")` also creates a local `log` module with
/// `log::error!`, `log::warn!`, `log::info!`, `log::debug!`, and `log::trace!`
/// macros that use the registered scope automatically.
#[doc(hidden)]
#[macro_export]
macro_rules! __policy_prompt {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        Ok(bindings::mitb::host::types::Action::Perturb(vec![
            bindings::mitb::host::types::Input::Text(format!($fmt $(, $args)*)),
            bindings::mitb::host::types::Input::Key(bindings::mitb::host::types::Key::Enter),
        ]))
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        Ok(bindings::mitb::host::types::Action::Perturb(vec![
            bindings::mitb::host::types::Input::Text(message.to_string()),
            bindings::mitb::host::types::Input::Key(bindings::mitb::host::types::Key::Enter),
        ]))
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_report_reward {
    ($reward:expr $(,)?) => {{
        report_reward(($reward) as f64).await?;
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_regex_capture {
    ($text:expr, $pattern:expr $(,)?) => {{ $crate::regex_capture($text, $pattern, 1) }};
    ($text:expr, $pattern:expr, $capture_index:expr $(,)?) => {{ $crate::regex_capture($text, $pattern, $capture_index) }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_regex_capture_first {
    ($text:expr, $pattern:expr $(,)?) => {{ $crate::regex_capture_first($text, $pattern, 1) }};
    ($text:expr, $pattern:expr, $capture_index:expr $(,)?) => {{ $crate::regex_capture_first($text, $pattern, $capture_index) }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_regex_captures {
    ($text:expr, $pattern:expr $(,)?) => {{ $crate::regex_captures($text, $pattern, 1) }};
    ($text:expr, $pattern:expr, $capture_index:expr $(,)?) => {{ $crate::regex_captures($text, $pattern, $capture_index) }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_log_error {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        let message = format!($fmt $(, $args)*);
        let _ = _sdk_log($crate::LogLevel::Error, POLICY_LOG_SCOPE, &message).await;
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        let _ = _sdk_log($crate::LogLevel::Error, POLICY_LOG_SCOPE, message).await;
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_log_warn {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        let message = format!($fmt $(, $args)*);
        let _ = _sdk_log($crate::LogLevel::Warn, POLICY_LOG_SCOPE, &message).await;
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        let _ = _sdk_log($crate::LogLevel::Warn, POLICY_LOG_SCOPE, message).await;
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_log_info {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        let message = format!($fmt $(, $args)*);
        let _ = _sdk_log($crate::LogLevel::Info, POLICY_LOG_SCOPE, &message).await;
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        let _ = _sdk_log($crate::LogLevel::Info, POLICY_LOG_SCOPE, message).await;
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_log_debug {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        let message = format!($fmt $(, $args)*);
        let _ = _sdk_log($crate::LogLevel::Debug, POLICY_LOG_SCOPE, &message).await;
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        let _ = _sdk_log($crate::LogLevel::Debug, POLICY_LOG_SCOPE, message).await;
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __policy_log_trace {
    ($fmt:literal $(, $args:expr)* $(,)?) => {{
        let message = format!($fmt $(, $args)*);
        let _ = _sdk_log($crate::LogLevel::Trace, POLICY_LOG_SCOPE, &message).await;
    }};
    ($message:expr $(,)?) => {{
        let message = $message;
        let message: &str = message.as_ref();
        let _ = _sdk_log($crate::LogLevel::Trace, POLICY_LOG_SCOPE, message).await;
    }};
}

#[macro_export]
macro_rules! policy {
    ($bindings:ident) => {
        #[allow(unused_imports)]
        use $crate::__policy_prompt as prompt;
        #[allow(unused_imports)]
        use $crate::__policy_regex_capture as regex_capture;
        #[allow(unused_imports)]
        use $crate::__policy_regex_capture_first as regex_capture_first;
        #[allow(unused_imports)]
        use $crate::__policy_regex_captures as regex_captures;
        #[allow(unused_imports)]
        use $crate::__policy_report_reward as report_reward;

        #[allow(dead_code)]
        fn init_logging() {
            let env = $bindings::wasi::cli::environment::get_environment();
            for (key, value) in env {
                if key == "RUST_LOG" {
                    if let Some(level) = $crate::parse_log_level(&value) {
                        $crate::set_max_log_level(level);
                    }
                    return;
                }
            }
        }

        #[allow(dead_code)]
        async fn _sdk_log(level: $crate::LogLevel, scope: &str, message: &str) -> Result<(), ()> {
            if !$crate::log_enabled(level) {
                return Ok(());
            }

            let prefix = $crate::level_str(level);
            let line = format!("{prefix} [{scope}] {message}\n");
            write_stderr(line.into_bytes()).await
        }

        #[allow(dead_code)]
        async fn log_error(scope: &str, message: &str) -> Result<(), ()> {
            _sdk_log($crate::LogLevel::Error, scope, message).await
        }

        #[allow(dead_code)]
        async fn log_warn(scope: &str, message: &str) -> Result<(), ()> {
            _sdk_log($crate::LogLevel::Warn, scope, message).await
        }

        #[allow(dead_code)]
        async fn log_info(scope: &str, message: &str) -> Result<(), ()> {
            _sdk_log($crate::LogLevel::Info, scope, message).await
        }

        #[allow(dead_code)]
        async fn log_debug(scope: &str, message: &str) -> Result<(), ()> {
            _sdk_log($crate::LogLevel::Debug, scope, message).await
        }

        #[allow(dead_code)]
        async fn log_trace(scope: &str, message: &str) -> Result<(), ()> {
            _sdk_log($crate::LogLevel::Trace, scope, message).await
        }

        #[allow(dead_code)]
        async fn print_line(message: &str) -> Result<(), ()> {
            let mut line = String::from(message);
            line.push('\n');
            write_stdout(line.into_bytes()).await
        }

        #[allow(dead_code)]
        async fn write_stdout(bytes: Vec<u8>) -> Result<(), ()> {
            let (mut tx, rx) = $bindings::wit_stream::new::<u8>();
            let (stream_result, write_result) = $crate::futures::join!(
                async { $bindings::wasi::cli::stdout::write_via_stream(rx).await },
                async {
                    let result = tx.write_all(bytes).await;
                    drop(tx);
                    result
                }
            );

            if stream_result.is_err() || !write_result.is_empty() {
                return Err(());
            }

            Ok(())
        }

        #[allow(dead_code)]
        async fn write_stderr(bytes: Vec<u8>) -> Result<(), ()> {
            let (mut tx, rx) = $bindings::wit_stream::new::<u8>();
            let (stream_result, write_result) = $crate::futures::join!(
                async { $bindings::wasi::cli::stderr::write_via_stream(rx).await },
                async {
                    let result = tx.write_all(bytes).await;
                    drop(tx);
                    result
                }
            );

            if stream_result.is_err() || !write_result.is_empty() {
                return Err(());
            }

            Ok(())
        }

        #[allow(dead_code)]
        async fn terminal_head() -> u64 {
            $bindings::mitb::host::terminal::head()
        }

        #[allow(dead_code)]
        async fn terminal_snapshot(max_bytes: u32) -> Result<Vec<u8>, String> {
            $bindings::mitb::host::terminal::snapshot(max_bytes).await
        }

        #[allow(dead_code)]
        async fn terminal_read_since(
            cursor: u64,
            max_bytes: u32,
        ) -> Result<(u64, Vec<u8>), String> {
            $bindings::mitb::host::terminal::read_since(cursor, max_bytes).await
        }

        #[allow(dead_code)]
        async fn terminal_read_since_text(
            cursor: u64,
            max_bytes: u32,
        ) -> Result<(u64, String), String> {
            let (next_cursor, bytes) = terminal_read_since(cursor, max_bytes).await?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            Ok((next_cursor, text))
        }

        #[allow(dead_code)]
        async fn terminal_snapshot_text(max_bytes: u32) -> Result<String, String> {
            let bytes = terminal_snapshot(max_bytes).await?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }

        #[allow(dead_code)]
        async fn report_reward(reward: f64) -> Result<(), String> {
            $bindings::mitb::host::reporting::report_reward(reward).await
        }

        #[allow(dead_code)]
        async fn run_process(name: &str, args: Vec<String>) -> Result<Vec<u8>, String> {
            match run_process_with_timeout(name, args, $crate::DEFAULT_PROCESS_TIMEOUT).await? {
                $crate::TimeoutOutcome::Completed(stdout) => Ok(stdout),
                $crate::TimeoutOutcome::TimedOut => Err(format!(
                    "process `{name}` timed out after {} seconds",
                    $crate::DEFAULT_PROCESS_TIMEOUT.as_secs()
                )),
            }
        }

        #[allow(dead_code)]
        async fn run_process_with_timeout(
            name: &str,
            args: Vec<String>,
            timeout: ::core::time::Duration,
        ) -> Result<$crate::TimeoutOutcome<Vec<u8>>, String> {
            let child = $bindings::mitb::host::process::spawn(name.to_string(), args)
                .await
                .map_err(|error| error.to_string())?;

            let (stdout, stdout_done) = child.read_stdout().await;
            let (stderr, stderr_done) = child.read_stderr().await;

            let stdout_fut = async move {
                let stdout_bytes = stdout.collect().await;
                stdout_done.into_future().await?;
                Ok::<Vec<u8>, String>(stdout_bytes)
            };
            let stderr_fut = async move {
                let stderr_bytes = stderr.collect().await;
                stderr_done.into_future().await?;
                Ok::<Vec<u8>, String>(stderr_bytes)
            };

            // Drain stdout/stderr while waiting so child processes writing large
            // output do not block on full pipes before exit or timeout.
            let wait_fut = async {
                match child
                    .wait_timeout($crate::duration_to_nanos_u64(timeout))
                    .await?
                {
                    Some(result) => {
                        Ok::<Option<$bindings::mitb::host::types::ExitStatus>, String>(Some(result))
                    }
                    None => {
                        let _ = child.kill().await;
                        Ok(None)
                    }
                }
            };
            let (wait_result, stdout_result, stderr_result) =
                $crate::futures::join!(wait_fut, stdout_fut, stderr_fut);
            let wait_result = match wait_result? {
                Some(wait_result) => wait_result,
                None => return Ok($crate::TimeoutOutcome::TimedOut),
            };
            let stdout_bytes = stdout_result?;
            let stderr_bytes = stderr_result?;

            if !wait_result.success {
                let status = wait_result
                    .code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| String::from("terminated by signal"));
                let stderr = String::from_utf8_lossy(&stderr_bytes);
                return Err(format!(
                    "process `{name}` exited with status {status}: {stderr}"
                ));
            }

            Ok($crate::TimeoutOutcome::Completed(stdout_bytes))
        }
    };
    ($bindings:ident, scope: $scope:expr) => {
        $crate::policy!($bindings);

        #[allow(dead_code)]
        const POLICY_LOG_SCOPE: &str = $scope;

        #[allow(dead_code)]
        mod log {
            pub(crate) use $crate::__policy_log_debug as debug;
            pub(crate) use $crate::__policy_log_error as error;
            pub(crate) use $crate::__policy_log_info as info;
            pub(crate) use $crate::__policy_log_trace as trace;
            pub(crate) use $crate::__policy_log_warn as warn;
        }
    };
}

/// Define a policy wrapper trait and bridge it into generated WIT bindings.
///
/// The generated trait is named `Policy`. It owns per-session state, while the
/// SDK wrapper manages shared poll mechanics such as idle detection.
#[macro_export]
macro_rules! policy_guest {
    ($bindings:ident) => {
        #[allow(dead_code)]
        type Action = $bindings::mitb::host::types::Action;
        type ActionResult = Result<Action, String>;

        trait Policy: 'static + Sized + Default {
            async fn act(&mut self, contents: String) -> ActionResult;
        }

        #[doc(hidden)]
        pub struct __PolicySessionState<T>
        where
            T: Policy,
        {
            idle: $crate::IdleTracker,
            policy: T,
        }

        impl<T> Default for __PolicySessionState<T>
        where
            T: Policy,
        {
            fn default() -> Self {
                init_logging();
                Self {
                    idle: $crate::IdleTracker::default(),
                    policy: T::default(),
                }
            }
        }

        #[doc(hidden)]
        pub struct __PolicySession<T>
        where
            T: Policy,
        {
            state: $crate::futures::lock::Mutex<__PolicySessionState<T>>,
        }

        impl<T> __PolicySession<T>
        where
            T: Policy,
        {
            fn new() -> Self {
                Self {
                    state: $crate::futures::lock::Mutex::new(__PolicySessionState::default()),
                }
            }

            async fn poll(&self) -> Result<$bindings::mitb::host::types::Action, String> {
                let contents = terminal_snapshot_text($crate::DEFAULT_TERMINAL_MAX_BYTES).await?;
                let mut state = self.state.lock().await;
                if !$crate::detect_idle(&mut state.idle, &contents) {
                    return Ok($bindings::mitb::host::types::Action::Wait);
                }

                state.policy.act(contents).await
            }
        }

        impl<T> $bindings::exports::mitb::host::policy_api::Guest for T
        where
            T: Policy,
        {
            type Session = __PolicySession<T>;
        }

        impl<T> $bindings::exports::mitb::host::policy_api::GuestSession for __PolicySession<T>
        where
            T: Policy,
        {
            fn new() -> Self {
                Self::new()
            }

            async fn poll(&self) -> Result<$bindings::mitb::host::types::Action, String> {
                self.poll().await
            }
        }
    };
}

/// Generate the full policy guest surface:
/// - common helper functions from [`policy!`]
/// - the `Policy` trait and WIT export bridge from [`policy_guest!`]
///
/// `policy_prelude!("...")` (or `policy_prelude!(scope: "...")`) also
/// registers a default log scope and
/// enables `log::error!`, `log::warn!`, `log::info!`, `log::debug!`, and
/// `log::trace!` macros in the policy module. The prelude also brings
/// `prompt!`, `report_reward!`, `regex_capture!`, `regex_capture_first!`,
/// `regex_captures!`, `Action`, and `ActionResult` into scope for policy code.
#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! policy_prelude {
    () => {
        mod bindings {
            include!(concat!(env!("OUT_DIR"), "/mitb_guest_bindgen.rs"));

            macro_rules! export_policy {
                        ($policy:ident) => {
                            crate::bindings::export!($policy with_types_in crate::bindings);
                        };
                    }

            pub(crate) use export_policy;
        }

        $crate::policy_prelude!(bindings);
    };
    ($scope:literal) => {
        $crate::policy_prelude!(scope: $scope);
    };
    (scope: $scope:expr) => {
        mod bindings {
            include!(concat!(env!("OUT_DIR"), "/mitb_guest_bindgen.rs"));

            macro_rules! export_policy {
                        ($policy:ident) => {
                            crate::bindings::export!($policy with_types_in crate::bindings);
                        };
                    }

            pub(crate) use export_policy;
        }

        $crate::policy_prelude!(bindings, scope: $scope);
    };
    ($bindings:ident) => {
        $crate::policy!($bindings);
        $crate::policy_guest!($bindings);
    };
    ($bindings:ident, $scope:literal) => {
        $crate::policy_prelude!($bindings, scope: $scope);
    };
    ($bindings:ident, scope: $scope:expr) => {
        $crate::policy!($bindings, scope: $scope);
        $crate::policy_guest!($bindings);
    };
}

/// Compatibility alias for prior naming.
#[macro_export]
macro_rules! controller_prelude {
    () => {
        $crate::policy_prelude!();
    };
    ($scope:literal) => {
        $crate::policy_prelude!($scope);
    };
    (scope: $scope:expr) => {
        $crate::policy_prelude!(scope: $scope);
    };
    ($bindings:ident) => {
        $crate::policy_prelude!($bindings);
    };
    ($bindings:ident, $scope:literal) => {
        $crate::policy_prelude!($bindings, $scope);
    };
    ($bindings:ident, scope: $scope:expr) => {
        $crate::policy_prelude!($bindings, scope: $scope);
    };
}

#[cfg(test)]
mod tests {
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use core::time::Duration;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        ExponentialBackoff, IdleTracker, LogLevel, MITB_HOME_DIR_ENV, TimeoutOutcome,
        detect_idle_at, home_dir_from_environment, parse_log_level, pseudo_shuffle, regex_capture,
        regex_capture_first, regex_captures, truncate, with_timeout,
    };

    #[test]
    fn parse_levels() {
        assert_eq!(parse_log_level("error"), Some(LogLevel::Error));
        assert_eq!(parse_log_level("warn"), Some(LogLevel::Warn));
        assert_eq!(parse_log_level("warning"), Some(LogLevel::Warn));
        assert_eq!(parse_log_level("info"), Some(LogLevel::Info));
        assert_eq!(parse_log_level("debug"), Some(LogLevel::Debug));
        assert_eq!(parse_log_level("trace"), Some(LogLevel::Trace));
        assert_eq!(parse_log_level("mitb=info"), Some(LogLevel::Info));
        assert_eq!(parse_log_level("mitb = TRACE"), Some(LogLevel::Trace));
        assert_eq!(parse_log_level("quiet"), None);
    }

    #[test]
    fn shuffle_is_deterministic() {
        let mut first = [1_u32, 2, 3, 4, 5, 6, 7];
        let mut second = [1_u32, 2, 3, 4, 5, 6, 7];

        pseudo_shuffle(&mut first, 12345);
        pseudo_shuffle(&mut second, 12345);

        assert_eq!(first, second);
    }

    #[test]
    fn exponential_backoff_advances_and_caps() {
        let mut backoff =
            ExponentialBackoff::new(Duration::from_millis(10), Duration::from_secs(1));

        assert_eq!(backoff.next_backoff(), Duration::from_millis(10));
        assert_eq!(backoff.next_backoff(), Duration::from_millis(20));
        assert_eq!(backoff.next_backoff(), Duration::from_millis(40));

        for _ in 0..8 {
            let _ = backoff.next_backoff();
        }

        assert_eq!(backoff.current_interval(), Duration::from_secs(1));
        assert_eq!(backoff.next_backoff(), Duration::from_secs(1));
    }

    #[test]
    fn exponential_backoff_reset_restores_initial_interval() {
        let mut backoff =
            ExponentialBackoff::new(Duration::from_millis(10), Duration::from_secs(1));
        let _ = backoff.next_backoff();
        let _ = backoff.next_backoff();

        backoff.reset();

        assert_eq!(backoff.current_interval(), Duration::from_millis(10));
        assert_eq!(backoff.next_backoff(), Duration::from_millis(10));
    }

    #[test]
    fn exponential_backoff_saturates_when_multiplier_overflows() {
        let mut backoff = ExponentialBackoff::new(Duration::MAX, Duration::from_nanos(123))
            .with_multiplier(u32::MAX);

        assert_eq!(backoff.next_backoff(), Duration::from_nanos(123));
        assert_eq!(backoff.current_interval(), Duration::from_nanos(123));
    }

    #[test]
    fn truncate_preserves_utf8_boundaries() {
        let truncated = truncate("héllo", 2);
        assert_eq!(truncated, "h...");

        let ascii = truncate("abcdef", 3);
        assert_eq!(ascii, "abc...");

        let unchanged = truncate("abc", 4);
        assert_eq!(unchanged, "abc");
    }

    #[test]
    fn regex_capture_returns_last_capture() {
        let text = "noise <guess>12</guess>\nmore <guess>42</guess>";
        assert_eq!(
            regex_capture(text, r"<guess>\s*([0-9]+)\s*</guess>", 1),
            Ok(Some("42".to_string()))
        );
    }

    #[test]
    fn regex_capture_first_returns_first_capture() {
        let text = "noise <guess>12</guess>\nmore <guess>42</guess>";
        assert_eq!(
            regex_capture_first(text, r"<guess>\s*([0-9]+)\s*</guess>", 1),
            Ok(Some("12".to_string()))
        );
    }

    #[test]
    fn regex_captures_returns_all_captures_in_order() {
        let text = "noise <guess>12</guess>\nmore <guess>42</guess>";
        assert_eq!(
            regex_captures(text, r"<guess>\s*([0-9]+)\s*</guess>", 1),
            Ok(vec!["12".to_string(), "42".to_string()])
        );
    }

    #[test]
    fn detect_idle_tracks_each_guest_type() {
        let mut first = IdleTracker::default();
        let mut second = IdleTracker::default();

        assert!(!detect_idle_at(&mut first, "same", 0, 5_000_000_000));
        assert!(!detect_idle_at(
            &mut first,
            "same",
            1_000_000_000,
            5_000_000_000
        ));
        assert!(!detect_idle_at(
            &mut first,
            "same",
            4_999_999_999,
            5_000_000_000
        ));
        assert!(detect_idle_at(
            &mut first,
            "same",
            5_000_000_000,
            5_000_000_000
        ));
        assert!(!detect_idle_at(
            &mut first,
            "different",
            5_100_000_000,
            5_000_000_000,
        ));

        assert!(!detect_idle_at(&mut second, "same", 10, 5_000_000_000));
        assert!(detect_idle_at(
            &mut second,
            "same",
            5_000_000_010,
            5_000_000_000,
        ));
    }

    #[test]
    fn home_dir_prefers_home() {
        let environment = vec![
            ("HOME".to_string(), "/home/chip".to_string()),
            ("USERPROFILE".to_string(), "C:\\Users\\chip".to_string()),
        ];

        assert_eq!(
            home_dir_from_environment(&environment),
            Ok(std::path::PathBuf::from("/home/chip"))
        );
    }

    #[test]
    fn home_dir_prefers_explicit_mitb_home_dir() {
        let environment = vec![
            (MITB_HOME_DIR_ENV.to_string(), "/explicit/home".to_string()),
            ("HOME".to_string(), "/home/chip".to_string()),
        ];

        assert_eq!(
            home_dir_from_environment(&environment),
            Ok(std::path::PathBuf::from("/explicit/home"))
        );
    }

    #[test]
    fn home_dir_falls_back_to_userprofile() {
        let environment = vec![("USERPROFILE".to_string(), "C:\\Users\\chip".to_string())];

        assert_eq!(
            home_dir_from_environment(&environment),
            Ok(std::path::PathBuf::from("C:\\Users\\chip"))
        );
    }

    #[test]
    fn home_dir_falls_back_to_homedrive_and_homepath() {
        let environment = vec![
            ("HOMEDRIVE".to_string(), "C:".to_string()),
            ("HOMEPATH".to_string(), "\\Users\\chip".to_string()),
        ];

        assert_eq!(
            home_dir_from_environment(&environment),
            Ok(std::path::PathBuf::from("C:\\Users\\chip"))
        );
    }

    #[test]
    fn home_dir_errors_when_no_home_variables_exist() {
        let environment = vec![("HOME".to_string(), String::new())];

        assert!(home_dir_from_environment(&environment).is_err());
    }

    #[test]
    fn with_timeout_returns_completed_value() {
        let result = futures::executor::block_on(with_timeout(
            futures::future::ready(7_u32),
            futures::future::pending::<()>(),
        ));

        assert_eq!(result, TimeoutOutcome::Completed(7));
    }

    #[test]
    fn with_timeout_times_out_and_drops_future() {
        struct NeverCompletes {
            dropped: Arc<AtomicBool>,
        }

        impl Future for NeverCompletes {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Pending
            }
        }

        impl Drop for NeverCompletes {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let result = futures::executor::block_on(with_timeout(
            NeverCompletes {
                dropped: dropped.clone(),
            },
            futures::future::ready(()),
        ));

        assert_eq!(result, TimeoutOutcome::TimedOut);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
