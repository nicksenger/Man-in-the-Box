use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use regex::Regex;

mod bindings {
    include!(concat!(env!("OUT_DIR"), "/treesitter_bindgen.rs"));
}

use bindings::exports::mitb::treesitter::api;

struct TreeSitterProvider;

#[derive(Clone, Debug)]
struct ParsedTree {
    language: String,
}

#[derive(Clone, Debug)]
enum QueryMode {
    Decision,
    Halstead,
}

#[derive(Clone, Debug)]
struct CompiledQuery {
    language: String,
    mode: QueryMode,
}

#[derive(Clone, Debug)]
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (index, byte) in source.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                starts.push(index + 1);
            }
        }
        Self { starts }
    }

    fn point_for_offset(&self, offset: usize) -> api::Point {
        let row = self.starts.partition_point(|start| *start <= offset);
        let row = row.saturating_sub(1);
        let column = offset.saturating_sub(self.starts[row]);
        api::Point {
            row: row as u32,
            column: column as u32,
        }
    }
}

static NEXT_TREE_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_QUERY_ID: AtomicU32 = AtomicU32::new(1);
static TREES: OnceLock<Mutex<HashMap<u32, ParsedTree>>> = OnceLock::new();
static QUERIES: OnceLock<Mutex<HashMap<u32, CompiledQuery>>> = OnceLock::new();

fn trees() -> &'static Mutex<HashMap<u32, ParsedTree>> {
    TREES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn queries() -> &'static Mutex<HashMap<u32, CompiledQuery>> {
    QUERIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn decision_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\b(?:if|match|for|while|loop)\b")
            .unwrap_or_else(|_| unreachable!("constant decision regex should compile"))
    })
}

fn operator_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"\b(?:if|match|for|while|loop|return|break|continue)\b|==|!=|<=|>=|&&|\|\||<<|>>|\+=|-=|\*=|/=|%=|&=|\|=|\^=|->|=>|[+\-*/%=&|!<>^]",
        )
        .unwrap_or_else(|_| unreachable!("constant operator regex should compile"))
    })
}

fn operand_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?x)
            [A-Za-z_][A-Za-z0-9_]*
            |
            \b\d+(?:\.\d+)?\b
            |
            "(?:\\.|[^"\\])*"
            |
            '(?:\\.|[^'\\])'
            "#,
        )
        .unwrap_or_else(|_| unreachable!("constant operand regex should compile"))
    })
}

fn capture(
    source: &str,
    line_index: &LineIndex,
    name: &str,
    start_byte: usize,
    end_byte: usize,
) -> api::Capture {
    let start = line_index.point_for_offset(start_byte);
    let end = line_index.point_for_offset(end_byte);
    let text = source.get(start_byte..end_byte).unwrap_or("").to_string();
    api::Capture {
        name: name.to_string(),
        span: api::Span {
            start_byte: start_byte as u32,
            end_byte: end_byte as u32,
            start,
            end,
        },
        text,
    }
}

impl api::Guest for TreeSitterProvider {
    fn list_languages() -> Vec<String> {
        vec![String::from("rust")]
    }

    fn parse(language: String, _source: String) -> Result<u32, api::ParseError> {
        if language != "rust" {
            return Err(api::ParseError::UnsupportedLanguage);
        }

        let tree_id = NEXT_TREE_ID.fetch_add(1, Ordering::Relaxed);
        trees()
            .lock()
            .map_err(|_| api::ParseError::Internal)?
            .insert(tree_id, ParsedTree { language });
        Ok(tree_id)
    }

    fn query_compile(language: String, query: String) -> Result<u32, api::QueryError> {
        if language != "rust" {
            return Err(api::QueryError::UnsupportedLanguage);
        }

        let mode = if query.contains("@decision") {
            QueryMode::Decision
        } else if query.contains("@operator") || query.contains("@operand") {
            QueryMode::Halstead
        } else {
            return Err(api::QueryError::InvalidQuery);
        };

        let query_id = NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        queries()
            .lock()
            .map_err(|_| api::QueryError::Internal)?
            .insert(query_id, CompiledQuery { language, mode });
        Ok(query_id)
    }

    fn query_exec(
        query_id: u32,
        tree_id: u32,
        source: String,
        limit: Option<u32>,
    ) -> Result<Vec<api::QueryMatch>, api::QueryError> {
        let query = {
            let guard = queries().lock().map_err(|_| api::QueryError::Internal)?;
            guard
                .get(&query_id)
                .cloned()
                .ok_or(api::QueryError::UnknownQuery)?
        };
        {
            let guard = trees().lock().map_err(|_| api::QueryError::Internal)?;
            let tree = guard.get(&tree_id).ok_or(api::QueryError::UnknownTree)?;
            if tree.language != query.language {
                return Err(api::QueryError::UnsupportedLanguage);
            }
        }

        let max_matches = limit.unwrap_or(u32::MAX) as usize;
        let line_index = LineIndex::new(source.as_str());
        let mut matches = Vec::new();

        match query.mode {
            QueryMode::Decision => {
                for found in decision_regex()
                    .find_iter(source.as_str())
                    .take(max_matches)
                {
                    matches.push(api::QueryMatch {
                        pattern_index: 0,
                        captures: vec![capture(
                            source.as_str(),
                            &line_index,
                            "decision",
                            found.start(),
                            found.end(),
                        )],
                    });
                }
            }
            QueryMode::Halstead => {
                let mut count = 0_usize;
                for found in operator_regex().find_iter(source.as_str()) {
                    if count >= max_matches {
                        break;
                    }
                    matches.push(api::QueryMatch {
                        pattern_index: 0,
                        captures: vec![capture(
                            source.as_str(),
                            &line_index,
                            "operator",
                            found.start(),
                            found.end(),
                        )],
                    });
                    count += 1;
                }
                for found in operand_regex().find_iter(source.as_str()) {
                    if count >= max_matches {
                        break;
                    }
                    matches.push(api::QueryMatch {
                        pattern_index: 1,
                        captures: vec![capture(
                            source.as_str(),
                            &line_index,
                            "operand",
                            found.start(),
                            found.end(),
                        )],
                    });
                    count += 1;
                }
            }
        }

        Ok(matches)
    }

    fn drop_tree(tree_id: u32) {
        if let Ok(mut guard) = trees().lock() {
            let _ = guard.remove(&tree_id);
        }
    }

    fn drop_query(query_id: u32) {
        if let Ok(mut guard) = queries().lock() {
            let _ = guard.remove(&query_id);
        }
    }
}

bindings::export!(TreeSitterProvider with_types_in bindings);
