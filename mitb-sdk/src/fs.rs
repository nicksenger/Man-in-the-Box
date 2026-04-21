use glob::{MatchOptions, Pattern};
use regex::RegexBuilder;
use wasip3::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorStat, DescriptorType, ErrorCode, OpenFlags, PathFlags,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMatch {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: DirEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateDirOutcome {
    Created,
    AlreadyExists,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathStat {
    pub kind: DirEntryKind,
    pub size: u64,
    pub data_modification_seconds: Option<i64>,
    pub status_change_seconds: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct FindOptions {
    pub include_files: bool,
    pub include_directories: bool,
    pub respect_gitignore: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            include_files: true,
            include_directories: false,
            respect_gitignore: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GrepOptions {
    pub path_pattern: String,
    pub case_sensitive: bool,
    pub literal: bool,
    pub respect_gitignore: bool,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            path_pattern: String::from("*"),
            case_sensitive: true,
            literal: false,
            respect_gitignore: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: u64,
    pub line: String,
}

#[derive(Clone, Debug)]
struct GitignoreRule {
    base_dir: String,
    pattern: Pattern,
    negated: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
}

struct PendingDirectory {
    descriptor: Descriptor,
    relative_path: String,
    inherited_rules_len: usize,
}

/// Find paths recursively from guest cwd with glob matching and `.gitignore` support.
pub async fn find(pattern: &str, options: FindOptions) -> Result<Vec<PathMatch>, String> {
    let include_pattern = Pattern::new(pattern)
        .map_err(|error| format!("invalid include pattern `{pattern}`: {error}"))?;
    let mut gitignore_rules = Vec::<GitignoreRule>::new();
    let mut stack = vec![PendingDirectory {
        descriptor: select_root_preopen()?,
        relative_path: String::new(),
        inherited_rules_len: 0,
    }];
    let mut matches = Vec::new();

    while let Some(directory) = stack.pop() {
        gitignore_rules.truncate(directory.inherited_rules_len);
        if options.respect_gitignore {
            load_gitignore_rules(
                &directory.descriptor,
                directory.relative_path.as_str(),
                &mut gitignore_rules,
            )
            .await?;
        }
        let inherited_rules_len = gitignore_rules.len();

        let entries =
            read_directory_entries(&directory.descriptor, directory.relative_path.as_str()).await?;
        for entry in entries {
            let is_directory = entry.type_ == DescriptorType::Directory;
            if options.respect_gitignore && entry.name == ".git" && is_directory {
                continue;
            }

            let child_relative_path =
                join_relative_path(directory.relative_path.as_str(), entry.name.as_str());
            if options.respect_gitignore
                && path_is_ignored(
                    gitignore_rules.as_slice(),
                    child_relative_path.as_str(),
                    is_directory,
                )
            {
                continue;
            }

            if is_directory {
                if options.include_directories
                    && matches_path_pattern(&include_pattern, child_relative_path.as_str())
                {
                    matches.push(PathMatch {
                        path: child_relative_path.clone(),
                        is_dir: true,
                    });
                }
                let child_directory = directory
                    .descriptor
                    .open_at(
                        PathFlags::empty(),
                        entry.name,
                        OpenFlags::DIRECTORY,
                        DescriptorFlags::READ,
                    )
                    .await
                    .map_err(|error| {
                        format!(
                            "failed opening directory `{}`: {}",
                            display_path(child_relative_path.as_str()),
                            error.name()
                        )
                    })?;
                stack.push(PendingDirectory {
                    descriptor: child_directory,
                    relative_path: child_relative_path,
                    inherited_rules_len,
                });
                continue;
            }

            if entry.type_ == DescriptorType::RegularFile
                && options.include_files
                && matches_path_pattern(&include_pattern, child_relative_path.as_str())
            {
                matches.push(PathMatch {
                    path: child_relative_path,
                    is_dir: false,
                });
            }
        }
    }

    Ok(matches)
}

/// Search file contents recursively from guest cwd.
///
/// `query` is treated as a regex by default, or as a literal when
/// `options.literal` is true.
pub async fn grep(query: &str, options: GrepOptions) -> Result<Vec<GrepMatch>, String> {
    let pattern = if options.literal {
        regex::escape(query)
    } else {
        query.to_string()
    };
    let regex = RegexBuilder::new(pattern.as_str())
        .case_insensitive(!options.case_sensitive)
        .build()
        .map_err(|error| format!("invalid grep query `{query}`: {error}"))?;

    let files = find(
        options.path_pattern.as_str(),
        FindOptions {
            include_files: true,
            include_directories: false,
            respect_gitignore: options.respect_gitignore,
        },
    )
    .await?;

    let mut matches = Vec::new();
    for file in files {
        let bytes = read(file.path.as_str()).await?;
        let text = String::from_utf8_lossy(bytes.as_slice());
        for (line_index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(GrepMatch {
                    path: file.path.clone(),
                    line_number: (line_index + 1) as u64,
                    line: line.to_string(),
                });
            }
        }
    }

    Ok(matches)
}

/// Read a file from guest cwd and return raw bytes.
pub async fn read(path: &str) -> Result<Vec<u8>, String> {
    let (file, display_name) = match open_file_for_read(path, MissingFileBehavior::Error).await? {
        OpenFileForReadOutcome::Found { file, display_path } => (file, display_path),
        OpenFileForReadOutcome::Missing { display_path: missing_path } => {
            return Err(format!(
                "failed opening file `{}`: {}",
                display_path(missing_path.as_str()),
                ErrorCode::NoEntry.name()
            ));
        }
    };
    read_file_descriptor(&file, display_name.as_str()).await
}

/// Read a UTF-8 text file from guest cwd.
pub async fn read_text(path: &str) -> Result<String, String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes)
        .map_err(|error| format!("file `{}` is not valid utf-8: {error}", display_path(path)))
}

/// Read a UTF-8 text file from guest cwd when it exists.
pub async fn read_text_if_exists(path: &str) -> Result<Option<String>, String> {
    let OpenFileForReadOutcome::Found {
        file,
        display_path: display_name,
    } =
        open_file_for_read(path, MissingFileBehavior::ReturnNone).await?
    else {
        return Ok(None);
    };
    let bytes = read_file_descriptor(&file, display_name.as_str()).await?;
    let text = String::from_utf8(bytes).map_err(|error| {
        format!(
            "file `{}` is not valid utf-8: {error}",
            display_path(display_name.as_str())
        )
    })?;
    Ok(Some(text))
}

/// Create a single directory.
///
/// This behaves like `mkdir`: it creates exactly one path segment and reports
/// whether the directory already existed.
pub async fn create_dir(path: &str) -> Result<CreateDirOutcome, String> {
    let resolved = resolve_path(path, true)?;
    if resolved.relative_path == "." {
        return Ok(CreateDirOutcome::AlreadyExists);
    }
    match resolved
        .root
        .create_directory_at(resolved.relative_path.clone())
        .await
    {
        Ok(()) => Ok(CreateDirOutcome::Created),
        Err(ErrorCode::Exist) => Ok(CreateDirOutcome::AlreadyExists),
        Err(error) => Err(format!(
            "failed creating directory `{}`: {}",
            display_path(resolved.display_path.as_str()),
            error.name()
        )),
    }
}

/// Recursively create directories from guest cwd.
pub async fn create_dir_all(path: &str) -> Result<(), String> {
    let normalized = normalize_path_for_resolution(path, true)?;
    if normalized == "." {
        return Ok(());
    }

    let mut current = String::new();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(format!(
                "parent path components are not supported: `{}`",
                display_path(path)
            ));
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        let _ = create_dir(current.as_str()).await?;
    }
    Ok(())
}

/// Write bytes to a file from guest cwd (create + truncate).
pub async fn write(path: &str, bytes: &[u8]) -> Result<(), String> {
    let resolved = resolve_path(path, false)?;
    if resolved.relative_path == "." {
        return Err(format!(
            "path must reference a file: `{}`",
            display_path(resolved.display_path.as_str())
        ));
    }
    let file = resolved
        .root
        .open_at(
            PathFlags::empty(),
            resolved.relative_path.clone(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE,
            DescriptorFlags::WRITE,
        )
        .await
        .map_err(|error| {
            format!(
                "failed opening file `{}` for write: {}",
                display_path(resolved.display_path.as_str()),
                error.name()
            )
        })?;
    write_file_descriptor(&file, resolved.display_path.as_str(), bytes.to_vec()).await
}

/// Write UTF-8 text to a file from guest cwd (create + truncate).
pub async fn write_text(path: &str, text: &str) -> Result<(), String> {
    write(path, text.as_bytes()).await
}

/// Write UTF-8 text atomically with temp-file + rename.
pub async fn write_text_atomic(path: &str, text: &str) -> Result<(), String> {
    let normalized = normalize_path_for_resolution(path, false)?;
    let now = wasip3::clocks::system_clock::now();
    let temp_path = format!(
        "{normalized}.tmp.{}-{}-{}",
        now.seconds,
        now.nanoseconds,
        wasip3::clocks::monotonic_clock::now()
    );
    write_text(temp_path.as_str(), text).await?;
    if let Err(error) = rename(temp_path.as_str(), normalized.as_str()).await {
        let _ = remove_file_if_exists(temp_path.as_str()).await;
        return Err(format!(
            "failed completing atomic write to `{}`: {error}",
            display_path(normalized.as_str())
        ));
    }
    Ok(())
}

/// Rename a filesystem entry.
pub async fn rename(old_path: &str, new_path: &str) -> Result<(), String> {
    let old_resolved = resolve_path(old_path, false)?;
    if old_resolved.relative_path == "." {
        return Err(format!(
            "source path must reference a file or directory entry: `{}`",
            display_path(old_resolved.display_path.as_str())
        ));
    }
    let new_resolved = resolve_path(new_path, false)?;
    if new_resolved.relative_path == "." {
        return Err(format!(
            "destination path must reference a file or directory entry: `{}`",
            display_path(new_resolved.display_path.as_str())
        ));
    }
    old_resolved
        .root
        .rename_at(
            old_resolved.relative_path.clone(),
            &new_resolved.root,
            new_resolved.relative_path.clone(),
        )
        .await
        .map_err(|error| {
            format!(
                "failed renaming `{}` to `{}`: {}",
                display_path(old_resolved.display_path.as_str()),
                display_path(new_resolved.display_path.as_str()),
                error.name()
            )
        })
}

/// Delete a non-directory file if it exists.
pub async fn remove_file_if_exists(path: &str) -> Result<(), String> {
    let resolved = resolve_path(path, false)?;
    if resolved.relative_path == "." {
        return Err(format!(
            "path must reference a file: `{}`",
            display_path(resolved.display_path.as_str())
        ));
    }
    match resolved
        .root
        .unlink_file_at(resolved.relative_path.clone())
        .await
    {
        Ok(()) | Err(ErrorCode::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "failed deleting file `{}`: {}",
            display_path(resolved.display_path.as_str()),
            error.name()
        )),
    }
}

/// List entries within a directory from guest cwd.
pub async fn read_dir(path: &str) -> Result<Vec<DirEntry>, String> {
    let normalized_path = normalize_path_for_resolution(path, true)?;
    let directory = open_directory(normalized_path.as_str()).await?;
    let entries = read_directory_entries(&directory, normalized_path.as_str()).await?;
    Ok(entries
        .into_iter()
        .map(|entry| DirEntry {
            name: entry.name,
            kind: descriptor_type_to_dir_entry_kind(entry.type_),
        })
        .collect())
}

/// Return metadata for a path, or `None` when it does not exist.
pub async fn stat(path: &str) -> Result<Option<PathStat>, String> {
    let resolved = resolve_path(path, true)?;
    let descriptor_stat = if resolved.relative_path == "." {
        resolved.root.stat().await.map_err(|error| {
            format!(
                "failed reading metadata for `{}`: {}",
                display_path(resolved.display_path.as_str()),
                error.name()
            )
        })?
    } else {
        match resolved
            .root
            .stat_at(PathFlags::empty(), resolved.relative_path.clone())
            .await
        {
            Ok(stat) => stat,
            Err(ErrorCode::NoEntry) => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "failed reading metadata for `{}`: {}",
                    display_path(resolved.display_path.as_str()),
                    error.name()
                ));
            }
        }
    };
    Ok(Some(path_stat_from_descriptor_stat(descriptor_stat)))
}

/// Return whole-second age for a path, or `None` when age cannot be computed.
pub async fn age_seconds(path: &str) -> Result<Option<u64>, String> {
    let Some(stat) = stat(path).await? else {
        return Ok(None);
    };
    let Some(modified_seconds) = stat.data_modification_seconds else {
        return Ok(None);
    };
    let now_seconds = wasip3::clocks::system_clock::now().seconds;
    let age_seconds = i128::from(now_seconds) - i128::from(modified_seconds);
    if age_seconds <= 0 {
        return Ok(Some(0));
    }
    Ok(Some(age_seconds as u64))
}

/// Count total line count for files matching a glob pattern recursively.
///
/// This uses [`find`] + [`read`] internally.
pub async fn count_lines(pattern: &str) -> Result<u64, String> {
    let files = find(
        pattern,
        FindOptions {
            include_files: true,
            include_directories: false,
            respect_gitignore: true,
        },
    )
    .await?;
    let mut total = 0_u64;
    for file in files {
        let contents = read(file.path.as_str()).await?;
        total = total.saturating_add(count_lines_in_bytes(contents.as_slice()));
    }
    Ok(total)
}

struct ResolvedPath {
    root: Descriptor,
    relative_path: String,
    display_path: String,
}

#[derive(Clone, Copy)]
enum MissingFileBehavior {
    Error,
    ReturnNone,
}

enum OpenFileForReadOutcome {
    Found { file: Descriptor, display_path: String },
    Missing { display_path: String },
}

async fn open_file_for_read(
    path: &str,
    missing_behavior: MissingFileBehavior,
) -> Result<OpenFileForReadOutcome, String> {
    let resolved = resolve_path(path, false)?;
    if resolved.relative_path == "." {
        return Err(format!(
            "path must reference a file: `{}`",
            display_path(resolved.display_path.as_str())
        ));
    }
    match resolved
        .root
        .open_at(
            PathFlags::empty(),
            resolved.relative_path.clone(),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
    {
        Ok(file) => Ok(OpenFileForReadOutcome::Found {
            file,
            display_path: resolved.display_path,
        }),
        Err(error) => match (error, missing_behavior) {
            (ErrorCode::NoEntry, MissingFileBehavior::ReturnNone) => {
                Ok(OpenFileForReadOutcome::Missing {
                    display_path: resolved.display_path,
                })
            }
            (other_error, MissingFileBehavior::Error | MissingFileBehavior::ReturnNone) => {
                Err(format!(
                    "failed opening file `{}`: {}",
                    display_path(resolved.display_path.as_str()),
                    other_error.name()
                ))
            }
        },
    }
}

async fn open_directory(path: &str) -> Result<Descriptor, String> {
    let resolved = resolve_path(path, true)?;
    if resolved.relative_path == "." {
        return Ok(resolved.root);
    }

    resolved
        .root
        .open_at(
            PathFlags::empty(),
            resolved.relative_path.clone(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ,
        )
        .await
        .map_err(|error| {
            format!(
                "failed opening directory `{}`: {}",
                display_path(resolved.display_path.as_str()),
                error.name()
            )
        })
}

async fn read_file_descriptor(file: &Descriptor, display_name: &str) -> Result<Vec<u8>, String> {
    let (contents, read_result) = file.read_via_stream(0);
    let contents = contents.collect().await;
    read_result.into_future().await.map_err(|error| {
        format!(
            "failed reading file `{}`: {}",
            display_path(display_name),
            error.name()
        )
    })?;
    Ok(contents)
}

async fn write_file_descriptor(
    file: &Descriptor,
    display_name: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let (mut tx, rx) = wasip3::wit_stream::new::<u8>();
    let (remaining, write_result) = futures::join!(
        async move {
            let remaining = tx.write_all(bytes).await;
            drop(tx);
            remaining
        },
        async move { file.write_via_stream(rx, 0).await }
    );
    if !remaining.is_empty() {
        return Err(format!(
            "failed writing file `{}`: {} bytes were not written",
            display_path(display_name),
            remaining.len()
        ));
    }
    write_result.map_err(|error| {
        format!(
            "failed writing file `{}`: {}",
            display_path(display_name),
            error.name()
        )
    })
}

async fn read_directory_entries(
    directory: &Descriptor,
    path: &str,
) -> Result<Vec<wasip3::filesystem::types::DirectoryEntry>, String> {
    let (entries, entries_result) = directory.read_directory().await;
    let entries = entries.collect().await;
    entries_result.into_future().await.map_err(|error| {
        format!(
            "failed reading directory `{}`: {}",
            display_path(path),
            error.name()
        )
    })?;
    Ok(entries)
}

fn normalize_path_for_resolution(path: &str, allow_current_dir: bool) -> Result<String, String> {
    let path = path.trim();
    if path.starts_with('/') {
        return Err(format!(
            "absolute paths are not supported in guest fs: `{path}`"
        ));
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty() || path == "." {
        if allow_current_dir {
            return Ok(String::from("."));
        }
        return Err(String::from("path must reference a file"));
    }
    Ok(path.to_string())
}

fn resolve_path(path: &str, allow_current_dir: bool) -> Result<ResolvedPath, String> {
    let normalized_path = normalize_path_for_resolution(path, allow_current_dir)?;
    let mut dot_root = None::<Descriptor>;
    let mut fallback_root = None::<Descriptor>;
    let mut best_match = None::<(usize, Descriptor, String)>;

    for (descriptor, preopen_path) in wasip3::filesystem::preopens::get_directories() {
        let mount = normalize_preopen_mount_path(preopen_path.as_str());
        if mount == "." {
            if dot_root.is_none() {
                dot_root = Some(descriptor);
            } else if fallback_root.is_none() {
                fallback_root = Some(descriptor);
            }
            continue;
        }

        let relative = if normalized_path == mount {
            Some(String::from("."))
        } else {
            normalized_path
                .strip_prefix(mount.as_str())
                .and_then(|remainder| remainder.strip_prefix('/'))
                .map(|remainder| {
                    if remainder.is_empty() {
                        String::from(".")
                    } else {
                        remainder.to_string()
                    }
                })
        };

        if let Some(relative) = relative {
            let mount_len = mount.len();
            let replace = best_match
                .as_ref()
                .map(|(prior_mount_len, _, _)| mount_len > *prior_mount_len)
                .unwrap_or(true);
            if replace {
                best_match = Some((mount_len, descriptor, relative));
            } else if fallback_root.is_none() {
                fallback_root = Some(descriptor);
            }
        } else if fallback_root.is_none() {
            fallback_root = Some(descriptor);
        }
    }

    if let Some((_mount_len, root, relative_path)) = best_match {
        return Ok(ResolvedPath {
            root,
            relative_path,
            display_path: normalized_path,
        });
    }
    if let Some(root) = dot_root.or(fallback_root) {
        return Ok(ResolvedPath {
            root,
            relative_path: normalized_path.clone(),
            display_path: normalized_path,
        });
    }
    Err(String::from(
        "wasi preopens are empty; no guest filesystem root",
    ))
}

fn normalize_preopen_mount_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "." {
        return String::from(".");
    }
    let trimmed = trimmed.trim_start_matches("./").trim_matches('/');
    if trimmed.is_empty() {
        String::from(".")
    } else {
        trimmed.to_string()
    }
}

fn path_stat_from_descriptor_stat(stat: DescriptorStat) -> PathStat {
    PathStat {
        kind: descriptor_type_to_dir_entry_kind(stat.type_),
        size: stat.size,
        data_modification_seconds: stat
            .data_modification_timestamp
            .map(|timestamp| timestamp.seconds),
        status_change_seconds: stat
            .status_change_timestamp
            .map(|timestamp| timestamp.seconds),
    }
}

fn descriptor_type_to_dir_entry_kind(descriptor_type: DescriptorType) -> DirEntryKind {
    match descriptor_type {
        DescriptorType::Directory => DirEntryKind::Directory,
        DescriptorType::RegularFile => DirEntryKind::File,
        DescriptorType::SymbolicLink => DirEntryKind::Symlink,
        _ => DirEntryKind::Other,
    }
}

fn select_root_preopen() -> Result<Descriptor, String> {
    let mut fallback = None;

    for (descriptor, path) in wasip3::filesystem::preopens::get_directories() {
        if path == "." {
            return Ok(descriptor);
        }
        if fallback.is_none() {
            fallback = Some(descriptor);
        }
    }

    fallback.ok_or_else(|| String::from("wasi preopens are empty; no guest filesystem root"))
}

async fn load_gitignore_rules(
    directory: &Descriptor,
    base_dir: &str,
    rules: &mut Vec<GitignoreRule>,
) -> Result<(), String> {
    let gitignore_descriptor = match directory
        .open_at(
            PathFlags::empty(),
            String::from(".gitignore"),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
    {
        Ok(file) => file,
        Err(ErrorCode::NoEntry) => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed opening `{}`: {}",
                display_path(join_relative_path(base_dir, ".gitignore").as_str()),
                error.name()
            ));
        }
    };

    let contents = read_file_descriptor(
        &gitignore_descriptor,
        join_relative_path(base_dir, ".gitignore").as_str(),
    )
    .await?;
    parse_gitignore_rules(
        String::from_utf8_lossy(contents.as_slice()).as_ref(),
        base_dir,
        rules,
    )
}

fn parse_gitignore_rules(
    contents: &str,
    base_dir: &str,
    rules: &mut Vec<GitignoreRule>,
) -> Result<(), String> {
    for original_line in contents.lines() {
        let mut line = original_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut negated = false;
        if let Some(stripped) = line.strip_prefix('!') {
            negated = true;
            line = stripped;
        }
        if line.is_empty() {
            continue;
        }

        let mut anchored = false;
        if let Some(stripped) = line.strip_prefix('/') {
            anchored = true;
            line = stripped;
        }
        if line.is_empty() {
            continue;
        }

        let mut directory_only = false;
        if let Some(stripped) = line.strip_suffix('/') {
            directory_only = true;
            line = stripped;
        }
        if line.is_empty() {
            continue;
        }

        let has_slash = line.contains('/');
        let normalized_pattern = line.replace('\\', "/");
        let pattern = Pattern::new(normalized_pattern.as_str()).map_err(|error| {
            format!(
                "invalid `.gitignore` pattern `{line}` under `{}`: {error}",
                display_path(base_dir)
            )
        })?;

        rules.push(GitignoreRule {
            base_dir: base_dir.to_string(),
            pattern,
            negated,
            directory_only,
            anchored,
            has_slash,
        });
    }

    Ok(())
}

fn path_is_ignored(rules: &[GitignoreRule], relative_path: &str, is_dir: bool) -> bool {
    let basename = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let mut ignored = false;

    for rule in rules {
        let path_from_base = if rule.base_dir.is_empty() {
            relative_path
        } else {
            let prefix = format!("{}/", rule.base_dir);
            if let Some(stripped) = relative_path.strip_prefix(prefix.as_str()) {
                stripped
            } else {
                continue;
            }
        };
        if path_from_base.is_empty() {
            continue;
        }

        let matches = if rule.directory_only {
            directory_prefixes(path_from_base, is_dir)
                .iter()
                .any(|directory_path| {
                    if rule.anchored || rule.has_slash {
                        glob_matches_pattern(&rule.pattern, directory_path)
                    } else {
                        let directory_basename =
                            directory_path.rsplit('/').next().unwrap_or(directory_path);
                        glob_matches_pattern(&rule.pattern, directory_basename)
                    }
                })
        } else if rule.anchored || rule.has_slash {
            glob_matches_pattern(&rule.pattern, path_from_base)
        } else {
            glob_matches_pattern(&rule.pattern, basename)
        };

        if matches {
            ignored = !rule.negated;
        }
    }

    ignored
}

fn matches_path_pattern(pattern: &Pattern, relative_path: &str) -> bool {
    if pattern.as_str().contains('/') {
        return glob_matches_pattern(pattern, relative_path);
    }

    let basename = relative_path.rsplit('/').next().unwrap_or(relative_path);
    glob_matches_pattern(pattern, basename)
}

fn glob_matches_pattern(pattern: &Pattern, text: &str) -> bool {
    let options = MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    pattern.matches_with(text, options)
}

fn join_relative_path(base: &str, child: &str) -> String {
    if base.is_empty() {
        return child.to_string();
    }

    format!("{base}/{child}")
}

fn display_path(path: &str) -> &str {
    if path.is_empty() { "." } else { path }
}

fn directory_prefixes(path: &str, is_dir: bool) -> Vec<&str> {
    let end = if is_dir {
        path.len()
    } else {
        path.rfind('/').unwrap_or_default()
    };
    if end == 0 {
        return Vec::new();
    }

    let directory_path = &path[..end];
    let mut prefixes = Vec::new();
    let mut search_from = 0;
    while let Some(relative_index) = directory_path[search_from..].find('/') {
        let slash_index = search_from + relative_index;
        prefixes.push(&directory_path[..slash_index]);
        search_from = slash_index + 1;
    }
    prefixes.push(directory_path);
    prefixes
}

fn count_lines_in_bytes(contents: &[u8]) -> u64 {
    if contents.is_empty() {
        return 0;
    }

    let newline_count = contents.iter().filter(|byte| **byte == b'\n').count() as u64;
    if contents.last() == Some(&b'\n') {
        newline_count
    } else {
        newline_count + 1
    }
}

#[cfg(test)]
mod tests {
    use super::{
        count_lines_in_bytes, matches_path_pattern, parse_gitignore_rules, path_is_ignored,
    };
    use glob::Pattern;

    #[test]
    fn include_pattern_without_separator_matches_basename_recursively() {
        let pattern = Pattern::new("*.rs");
        assert!(pattern.is_ok());
        if let Ok(pattern) = pattern {
            assert!(matches_path_pattern(&pattern, "src/lib.rs"));
            assert!(!matches_path_pattern(&pattern, "src/lib.ts"));
        }
    }

    #[test]
    fn gitignore_rules_ignore_and_unignore_paths() {
        let mut rules = Vec::new();
        assert_eq!(
            parse_gitignore_rules("target/\n*.tmp\n!important.tmp\n", "", &mut rules),
            Ok(())
        );

        assert!(path_is_ignored(&rules, "target", true));
        assert!(path_is_ignored(&rules, "target/output.bin", false));
        assert!(path_is_ignored(&rules, "foo.tmp", false));
        assert!(!path_is_ignored(&rules, "important.tmp", false));
    }

    #[test]
    fn gitignore_rules_scope_to_their_directory() {
        let mut rules = Vec::new();
        assert_eq!(
            parse_gitignore_rules("generated/\n", "src", &mut rules),
            Ok(())
        );

        assert!(path_is_ignored(&rules, "src/generated", true));
        assert!(path_is_ignored(&rules, "src/generated/file.rs", false));
        assert!(!path_is_ignored(&rules, "generated/file.rs", false));
    }

    #[test]
    fn count_lines_in_bytes_handles_newline_and_non_newline_endings() {
        assert_eq!(count_lines_in_bytes(b""), 0);
        assert_eq!(count_lines_in_bytes(b"a"), 1);
        assert_eq!(count_lines_in_bytes(b"a\n"), 1);
        assert_eq!(count_lines_in_bytes(b"a\nb\n"), 2);
        assert_eq!(count_lines_in_bytes(b"a\nb"), 2);
    }
}
