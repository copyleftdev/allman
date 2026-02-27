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

        if let Err(e) = self.ensure_repo() {
            tracing::error!("Failed to initialize git repo: {:?}", e);
            return;
        }

        while let Ok(req) = self.receiver.recv() {
            match req {
                GitRequest::CommitFile {
                    path,
                    content,
                    message,
                } => {
                    if let Err(e) = self.handle_commit(&path, &content, &message) {
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
            let repo = Repository::init(&self.repo_root).context("Failed to init repo")?;

            // Create initial commit if empty?
            Ok(repo)
        }
    }

    fn handle_commit(&self, rel_path: &str, content: &str, msg: &str) -> anyhow::Result<()> {
        let repo = Repository::open(&self.repo_root)?;
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
