use core::future::Future;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRepo {
    repo_dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSnapshot {
    pub sha: String,
    pub branch: Option<String>,
}

impl GitRepo {
    pub fn new(repo_dir: impl Into<String>) -> Self {
        Self {
            repo_dir: repo_dir.into(),
        }
    }

    pub fn repo_dir(&self) -> &str {
        self.repo_dir.as_str()
    }

    pub async fn head_sha<R, Fut>(&self, run_process: &R) -> Result<String, String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        let output = self
            .run(
                run_process,
                vec!["rev-parse".to_string(), "HEAD".to_string()],
            )
            .await?;
        parse_non_empty_utf8(output, "git rev-parse HEAD")
    }

    pub async fn current_branch<R, Fut>(&self, run_process: &R) -> Result<Option<String>, String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        let output = self
            .run(
                run_process,
                vec!["branch".to_string(), "--show-current".to_string()],
            )
            .await?;
        parse_optional_utf8(output, "git branch --show-current")
    }

    pub async fn snapshot<R, Fut>(&self, run_process: &R) -> Result<GitSnapshot, String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        Ok(GitSnapshot {
            sha: self.head_sha(run_process).await?,
            branch: self.current_branch(run_process).await?,
        })
    }

    pub async fn checkout_force<R, Fut>(&self, run_process: &R, rev: &str) -> Result<(), String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        self.run(
            run_process,
            vec![
                "checkout".to_string(),
                "--force".to_string(),
                rev.to_string(),
            ],
        )
        .await
        .map(|_| ())
    }

    pub async fn switch_create_or_reset<R, Fut>(
        &self,
        run_process: &R,
        branch: &str,
        start_point: &str,
    ) -> Result<(), String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        self.run(
            run_process,
            vec![
                "checkout".to_string(),
                "--force".to_string(),
                "-B".to_string(),
                branch.to_string(),
                start_point.to_string(),
            ],
        )
        .await
        .map(|_| ())
    }

    pub async fn add_all<R, Fut>(&self, run_process: &R) -> Result<(), String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        self.run(run_process, vec!["add".to_string(), "-A".to_string()])
            .await
            .map(|_| ())
    }

    pub async fn commit_all<R, Fut>(
        &self,
        run_process: &R,
        message: &str,
        allow_empty: bool,
    ) -> Result<String, String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        let mut args = vec![
            "-c".to_string(),
            "commit.gpgsign=false".to_string(),
            "commit".to_string(),
        ];
        if allow_empty {
            args.push("--allow-empty".to_string());
        }
        args.push("-m".to_string());
        args.push(message.to_string());

        self.run(run_process, args).await?;
        self.head_sha(run_process).await
    }

    async fn run<R, Fut>(&self, run_process: &R, mut args: Vec<String>) -> Result<Vec<u8>, String>
    where
        R: Fn(String, Vec<String>) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>>,
    {
        let mut command = vec!["-C".to_string(), self.repo_dir.clone()];
        command.append(&mut args);
        run_process("git".to_string(), command).await
    }
}

fn parse_non_empty_utf8(bytes: Vec<u8>, command_label: &str) -> Result<String, String> {
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("{command_label} output was not utf-8: {error}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{command_label} returned an empty string"));
    }
    Ok(trimmed.to_string())
}

fn parse_optional_utf8(bytes: Vec<u8>, command_label: &str) -> Result<Option<String>, String> {
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("{command_label} output was not utf-8: {error}"))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::GitRepo;

    #[test]
    fn head_sha_runs_git_in_repo_directory() {
        let repo = GitRepo::new("/workspace/repo");
        let calls = Rc::new(RefCell::new(Vec::<(String, Vec<String>)>::new()));
        let calls_for_run = calls.clone();
        let run = move |name: String, args: Vec<String>| {
            let calls_for_run = calls_for_run.clone();
            async move {
                calls_for_run.borrow_mut().push((name, args));
                Ok(b"abc123\n".to_vec())
            }
        };

        let sha = futures::executor::block_on(repo.head_sha(&run)).unwrap();

        assert_eq!(sha, "abc123");
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].0, "git");
        assert_eq!(
            calls.borrow()[0].1,
            vec![
                "-C".to_string(),
                "/workspace/repo".to_string(),
                "rev-parse".to_string(),
                "HEAD".to_string(),
            ]
        );
    }

    #[test]
    fn branch_name_is_optional_when_detached() {
        let repo = GitRepo::new(".");
        let run = |_name: String, _args: Vec<String>| async { Ok(Vec::new()) };

        let branch = futures::executor::block_on(repo.current_branch(&run)).unwrap();

        assert_eq!(branch, None);
    }

    #[test]
    fn commit_all_returns_new_head_sha() {
        let repo = GitRepo::new(".");
        let calls = Rc::new(RefCell::new(Vec::<(String, Vec<String>)>::new()));
        let calls_for_run = calls.clone();
        let run = move |name: String, args: Vec<String>| {
            let calls_for_run = calls_for_run.clone();
            async move {
                calls_for_run.borrow_mut().push((name, args.clone()));
                if args.iter().any(|arg| arg == "rev-parse") {
                    Ok(b"nextsha\n".to_vec())
                } else {
                    Ok(Vec::new())
                }
            }
        };

        let sha = futures::executor::block_on(repo.commit_all(&run, "message", true)).unwrap();

        assert_eq!(sha, "nextsha");
        assert_eq!(calls.borrow().len(), 2);
        assert!(calls.borrow()[0].1.contains(&"commit".to_string()));
        assert!(calls.borrow()[0].1.contains(&"--allow-empty".to_string()));
    }
}
