use anyhow::Context;
use crossbeam_channel::Receiver;
use git2::{Repository, Signature};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum GitRequest {
    /// Write content to a file and commit it.
    /// path: relative to repo root
    /// content: string content
    /// message: commit message
    CommitFile {
        path: String,
        content: String,
        message: String,
    },
}

pub struct GitActor {
    repo_root: PathBuf,
    receiver: Receiver<GitRequest>,
}

impl GitActor {
    pub fn new(repo_root: PathBuf, receiver: Receiver<GitRequest>) -> Self {
        Self {
            repo_root,
            receiver,
        }
    }

    /// Run the actor on a dedicated OS thread. git2 is synchronous —
    /// this must never execute on the tokio runtime.
    pub fn run(self) {
        tracing::info!("GitActor started for {:?}", self.repo_root);

        let repo = match self.ensure_repo() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to initialize git repo: {:?}", e);
                return;
            }
        };

        while let Ok(req) = self.receiver.recv() {
            match req {
                GitRequest::CommitFile {
                    path,
                    content,
                    message,
                } => {
                    if let Err(e) = self.handle_commit(&repo, &path, &content, &message) {
                        tracing::error!("Failed to commit file {}: {:?}", path, e);
                    }
                }
            }
        }
        tracing::info!("GitActor shutting down (channel closed)");
    }

    fn ensure_repo(&self) -> anyhow::Result<Repository> {
        if self.repo_root.join(".git").exists() {
            Repository::open(&self.repo_root).context("Failed to open existing repo")
        } else {
            std::fs::create_dir_all(&self.repo_root)?;
            Repository::init(&self.repo_root).context("Failed to init repo")
        }
    }

    fn handle_commit(
        &self,
        repo: &Repository,
        rel_path: &str,
        content: &str,
        msg: &str,
    ) -> anyhow::Result<()> {
        // Defense-in-depth: reject paths that escape the repo root.
        let rel = Path::new(rel_path);
        anyhow::ensure!(!rel.is_absolute(), "Refusing absolute path: {}", rel_path);
        for component in rel.components() {
            anyhow::ensure!(
                !matches!(component, std::path::Component::ParentDir),
                "Refusing path with '..': {}",
                rel_path
            );
        }

        let full_path = self.repo_root.join(rel_path);

        // Ensure parent dirs
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write file
        std::fs::write(&full_path, content)?;

        // Git plumbing
        let mut index = repo.index()?;
        index.add_path(Path::new(rel_path))?;
        index.write()?;

        let oid = index.write_tree()?;
        let tree = repo.find_tree(oid)?;

        let sig = Signature::now("allman", "allman@localhost")?;

        let parent_commit = match repo.head() {
            Ok(head) => Some(head.peel_to_commit()?),
            Err(_) => None, // Initial commit
        };
        let parents = match &parent_commit {
            Some(c) => vec![c],
            None => vec![],
        };

        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)?;

        tracing::info!(" committed: {}", rel_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── H9: Path traversal must be BLOCKED ─────────────────────────────
    // Defense-in-depth: handle_commit must reject paths that resolve
    // outside the repo root.
    /// Helper: create actor + repo for testing.
    fn test_actor() -> (GitActor, Repository, TempDir) {
        let repo_dir = TempDir::new().unwrap();
        let (_tx, rx) = crossbeam_channel::bounded(10);
        let actor = GitActor::new(repo_dir.path().to_path_buf(), rx);
        let repo = actor.ensure_repo().unwrap();
        (actor, repo, repo_dir)
    }

    #[test]
    fn h9_path_traversal_is_blocked() {
        let (actor, repo, repo_dir) = test_actor();
        let escape_target = repo_dir.path().parent().unwrap().join("escaped_file.txt");

        let result = actor.handle_commit(&repo, "../escaped_file.txt", "pwned", "escape attempt");
        assert!(result.is_err(), "Path traversal with .. must be rejected");
        assert!(
            !escape_target.exists(),
            "No file must be written outside repo root"
        );
    }

    #[test]
    fn h9_deep_traversal_is_blocked() {
        let (actor, repo, _dir) = test_actor();

        let result =
            actor.handle_commit(&repo, "agents/../../etc/passwd", "pwned", "deep traversal");
        assert!(result.is_err(), "Deep path traversal must be rejected");
    }

    #[test]
    fn h9_absolute_path_is_blocked() {
        let (actor, repo, _dir) = test_actor();

        let result = actor.handle_commit(&repo, "/etc/passwd", "pwned", "absolute path");
        assert!(result.is_err(), "Absolute paths must be rejected");
    }

    #[test]
    fn h9_valid_nested_path_still_works() {
        let (actor, repo, repo_dir) = test_actor();

        let result = actor.handle_commit(
            &repo,
            "agents/alice/profile.json",
            "ok",
            "valid nested path",
        );
        assert!(result.is_ok(), "Valid nested paths must still work");
        assert!(repo_dir.path().join("agents/alice/profile.json").exists());
    }

    // ── H7: Repo handle reused across commits ──────────────────────────
    #[test]
    fn h7_sequential_commits_with_reused_handle() {
        let (actor, repo, repo_dir) = test_actor();

        actor
            .handle_commit(&repo, "file1.txt", "content1", "first commit")
            .expect("First commit should succeed");

        actor
            .handle_commit(&repo, "file2.txt", "content2", "second commit")
            .expect("Second commit should succeed");

        actor
            .handle_commit(&repo, "file1.txt", "updated", "update file1")
            .expect("Third commit should succeed");

        assert_eq!(
            std::fs::read_to_string(repo_dir.path().join("file1.txt")).unwrap(),
            "updated"
        );
        assert_eq!(
            std::fs::read_to_string(repo_dir.path().join("file2.txt")).unwrap(),
            "content2"
        );
    }

    // ── H8: Initial commit on empty repo ────────────────────────────────
    #[test]
    fn h8_initial_commit_on_empty_repo() {
        let (actor, repo, repo_dir) = test_actor();

        let result = actor.handle_commit(&repo, "init.txt", "hello", "initial commit");
        assert!(
            result.is_ok(),
            "Initial commit on empty repo should succeed"
        );

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        assert_eq!(commit.message().unwrap(), "initial commit");
        assert!(repo_dir.path().join("init.txt").exists());
    }
}
