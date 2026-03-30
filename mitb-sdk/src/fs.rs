use glob::{MatchOptions, Pattern};
use regex::RegexBuilder;
use wasip3::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags,
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
    let normalized_path = normalize_relative_path(path)?;
    let root = select_root_preopen()?;
    let file = root
        .open_at(
            PathFlags::empty(),
            normalized_path.to_string(),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .map_err(|error| {
            format!(
                "failed opening file `{}`: {}",
                display_path(normalized_path),
                error.name()
            )
        })?;
    read_file_descriptor(&file, normalized_path).await
}

/// Read a UTF-8 text file from guest cwd.
pub async fn read_text(path: &str) -> Result<String, String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes)
        .map_err(|error| format!("file `{}` is not valid utf-8: {error}", display_path(path)))
}

/// List entries within a directory from guest cwd.
pub async fn read_dir(path: &str) -> Result<Vec<DirEntry>, String> {
    let normalized_path = normalize_dir_path(path);
    let directory = open_directory(normalized_path).await?;
    let entries = read_directory_entries(&directory, normalized_path).await?;
    Ok(entries
        .into_iter()
        .map(|entry| DirEntry {
            name: entry.name,
            kind: descriptor_type_to_dir_entry_kind(entry.type_),
        })
        .collect())
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

async fn open_directory(path: &str) -> Result<Descriptor, String> {
    let root = select_root_preopen()?;
    if path.is_empty() || path == "." {
        return Ok(root);
    }

    root.open_at(
        PathFlags::empty(),
        path.to_string(),
        OpenFlags::DIRECTORY,
        DescriptorFlags::READ,
    )
    .await
    .map_err(|error| {
        format!(
            "failed opening directory `{}`: {}",
            display_path(path),
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

fn normalize_relative_path(path: &str) -> Result<&str, String> {
    let path = path.trim();
    if path.is_empty() || path == "." {
        return Err(String::from("path must reference a file"));
    }
    if path.starts_with('/') {
        return Err(format!(
            "absolute paths are not supported in guest fs: `{path}`"
        ));
    }

    Ok(path.strip_prefix("./").unwrap_or(path))
}

fn normalize_dir_path(path: &str) -> &str {
    let path = path.trim();
    if path.is_empty() {
        return ".";
    }
    path.strip_prefix("./").unwrap_or(path)
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
