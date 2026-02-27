use crate::git_actor::{GitActor, GitRequest};
use crate::models::InboxEntry;
use anyhow::Context;
use crossbeam_channel::{bounded, Sender as CbSender};
use dashmap::DashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tantivy::{schema::*, Index, IndexReader};
use tokio::task;

// ── NRT Shutdown Guard ──────────────────────────────────────────────────────
// Sets a shared AtomicBool flag when the last Arc reference is dropped.
// This signals the NRT reader refresh task to exit its loop.
struct NrtShutdownGuard(Arc<AtomicBool>);

impl Drop for NrtShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// ── Persistence Pipeline ─────────────────────────────────────────────────────
// Operations dispatched from the hot path to the background persistence worker.
// The hot path returns to the client immediately after DashMap mutation + channel send.
#[derive(Debug)]
pub enum PersistOp {
    IndexMessage {
        id: String,
        project_id: String,
        from_agent: String,
        to_recipients: String,
        subject: String,
        body: String,
        created_ts: i64,
    },
    GitCommit {
        path: String,
        content: String,
        message: String,
    },
}

// ── Agent Record (stored in DashMap) ─────────────────────────────────────────
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentRecord {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub program: String,
    pub model: String,
}

// ── Core State ───────────────────────────────────────────────────────────────
// All hot-path reads and writes go through lock-free DashMap operations.
// Persistence (Tantivy indexing, Git commits) is async via crossbeam channel.
#[derive(Clone)]
pub struct PostOffice {
    // Lock-free concurrent state (microsecond access)
    pub projects: Arc<DashMap<String, String>>, // project_id → human_key
    pub agents: Arc<DashMap<String, AgentRecord>>, // agent_name → record
    pub inboxes: Arc<DashMap<String, Vec<InboxEntry>>>, // agent_name → unread messages

    // Tantivy search (read path only — writes go through persist pipeline)
    pub index: Arc<Index>,
    pub index_reader: IndexReader,

    // Async persistence pipeline (fire-and-forget from hot path)
    pub persist_tx: CbSender<PersistOp>,

    // NRT shutdown guard — when the last PostOffice clone is dropped,
    // the guard sets a flag that stops the NRT reader refresh task.
    _nrt_guard: Arc<NrtShutdownGuard>,
}

impl PostOffice {
    pub fn new(index_path: &Path, repo_root: &Path) -> anyhow::Result<Self> {
        // ── 1. Tantivy Search Index ──────────────────────────────────────────
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("project_id", STRING | STORED);
        schema_builder.add_text_field("from_agent", STRING | STORED);
        schema_builder.add_text_field("to_recipients", STRING | STORED);
        schema_builder.add_text_field("subject", TEXT | STORED);
        schema_builder.add_text_field("body", TEXT | STORED);
        schema_builder.add_i64_field("created_ts", INDEXED | STORED);
        schema_builder.add_text_field("id", STRING | STORED);
        let schema = schema_builder.build();

        std::fs::create_dir_all(index_path).context("Failed to create index directory")?;

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(index_path)?,
            schema.clone(),
        )
        .context("Failed to open tantivy index")?;

        let index_writer = index.writer(50_000_000)?;
        let index_reader = index.reader()?;

        // ── 2. Git Actor (dedicated OS thread — git2 is sync) ────────────────
        let (git_tx, git_rx) = bounded::<GitRequest>(10_000);
        let actor = GitActor::new(repo_root.to_path_buf(), git_rx);
        std::thread::Builder::new()
            .name("git-actor".into())
            .spawn(move || actor.run())
            .context("Failed to spawn git actor thread")?;

        // ── 3. Persistence Worker (dedicated OS thread) ──────────────────────
        // Owns the Tantivy IndexWriter exclusively. Batches operations for throughput.
        // Pattern: block on first op → drain pending → commit batch.
        let (persist_tx, persist_rx) = bounded::<PersistOp>(100_000);
        {
            let schema = schema.clone();
            let git_tx = git_tx.clone();
            std::thread::Builder::new()
                .name("persist-worker".into())
                .spawn(move || {
                    persistence_worker(persist_rx, index_writer, schema, git_tx);
                })
                .context("Failed to spawn persistence worker")?;
        }

        // ── 4. NRT Reader Refresh (lightweight tokio task) ───────────────────
        // Uses a shared AtomicBool flag for clean shutdown. When the last
        // PostOffice clone is dropped, NrtShutdownGuard sets the flag → task exits.
        let nrt_shutdown = Arc::new(AtomicBool::new(false));
        let nrt_guard = Arc::new(NrtShutdownGuard(nrt_shutdown.clone()));
        let reader_clone = index_reader.clone();
        let shutdown_flag = nrt_shutdown;
        task::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if shutdown_flag.load(Ordering::Relaxed) {
                    tracing::info!("NRT reader refresh task shutting down");
                    break;
                }
                if let Err(e) = reader_clone.reload() {
                    tracing::error!("NRT reader reload failed: {}", e);
                }
            }
        });

        tracing::info!(
            "PostOffice initialized: DashMap state, crossbeam persist pipeline, Tantivy NRT search"
        );

        Ok(PostOffice {
            projects: Arc::new(DashMap::new()),
            agents: Arc::new(DashMap::new()),
            inboxes: Arc::new(DashMap::new()),
            index: Arc::new(index),
            index_reader,
            persist_tx,
            _nrt_guard: nrt_guard,
        })
    }
}

// ── Persistence Worker ───────────────────────────────────────────────────────
// Runs on a dedicated OS thread. Owns the Tantivy IndexWriter exclusively.
// Batches incoming operations for maximum write throughput.
//
// Gjengset pattern: block → drain → commit. One commit per batch instead of
// one commit per document yields ~100x throughput improvement.
fn persistence_worker(
    rx: crossbeam_channel::Receiver<PersistOp>,
    mut index_writer: tantivy::IndexWriter,
    schema: Schema,
    git_tx: CbSender<GitRequest>,
) {
    tracing::info!("Persistence worker started");

    let f_id = schema.get_field("id").unwrap();
    let f_project = schema.get_field("project_id").unwrap();
    let f_from = schema.get_field("from_agent").unwrap();
    let f_to = schema.get_field("to_recipients").unwrap();
    let f_subject = schema.get_field("subject").unwrap();
    let f_body = schema.get_field("body").unwrap();
    let f_ts = schema.get_field("created_ts").unwrap();

    loop {
        // Block until the first operation arrives
        let first = match rx.recv() {
            Ok(op) => op,
            Err(_) => {
                tracing::info!("Persistence worker shutting down (channel closed)");
                break;
            }
        };

        // Drain all pending ops (non-blocking) up to batch limit
        let mut batch = Vec::with_capacity(1024);
        batch.push(first);
        while batch.len() < 4096 {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }

        let mut indexed = 0usize;

        for op in batch {
            match op {
                PersistOp::IndexMessage {
                    id,
                    project_id,
                    from_agent,
                    to_recipients,
                    subject,
                    body,
                    created_ts,
                } => {
                    let mut doc = tantivy::Document::default();
                    doc.add_text(f_id, &id);
                    doc.add_text(f_project, &project_id);
                    doc.add_text(f_from, &from_agent);
                    doc.add_text(f_to, &to_recipients);
                    doc.add_text(f_subject, &subject);
                    doc.add_text(f_body, &body);
                    doc.add_i64(f_ts, created_ts);
                    if let Err(e) = index_writer.add_document(doc) {
                        tracing::error!("Tantivy add_document failed: {}", e);
                    }
                    indexed += 1;
                }
                PersistOp::GitCommit {
                    path,
                    content,
                    message,
                } => {
                    if let Err(e) = git_tx.try_send(GitRequest::CommitFile {
                        path: path.clone(),
                        content,
                        message,
                    }) {
                        tracing::warn!("Git channel full, dropping commit for {}: {}", path, e);
                    }
                }
            }
        }

        // Single commit for the entire batch
        if indexed > 0 {
            if let Err(e) = index_writer.commit() {
                tracing::error!("Tantivy batch commit failed ({} docs): {}", indexed, e);
            }
        }
    }
}
