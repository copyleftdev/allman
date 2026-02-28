use crate::git_actor::{GitActor, GitRequest};
use crate::models::InboxEntry;
use anyhow::Context;
use crossbeam_channel::{bounded, Sender as CbSender};
use dashmap::DashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tantivy::{schema::*, Index, IndexReader};
use tokio::task;

// ── NRT Shutdown Guard ──────────────────────────────────────────────────────
// Sets a shared AtomicBool flag when the last Arc reference is dropped.
// This signals the NRT reader refresh task to exit its loop.
struct NrtShutdownGuard(Arc<AtomicBool>);

impl Drop for NrtShutdownGuard {
    fn drop(&mut self) {
        // Release ensures the store is visible to the Acquire load in the
        // NRT refresh task, providing a proper happens-before relationship
        // on weak memory architectures (ARM, RISC-V) (DR36-H2).
        self.0.store(true, Ordering::Release);
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
// Fields are populated during create_agent and stored for future API extensions
// (e.g., agent listing, admin endpoints). Currently only `name` (the DashMap key)
// is read on hot paths — send_message uses contains_key(), get_inbox uses the
// inboxes map. The remaining fields are write-only for now (DR40-H4).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields stored for future API extensions (DR40-H4)
pub struct AgentRecord {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub program: String,
    pub model: String,
    pub registered_at: i64,
}

// ── Pre-resolved Tantivy Field Handles ───────────────────────────────────────
// Resolved once at startup. Avoids 7 HashMap lookups per search_messages
// request via schema.get_field() (DR37-H3).
#[derive(Clone, Copy)]
pub struct SearchFields {
    pub id: Field,
    pub project_id: Field,
    pub from_agent: Field,
    pub to_recipients: Field,
    pub subject: Field,
    pub body: Field,
    pub created_ts: Field,
}

// ── Core State ───────────────────────────────────────────────────────────────
// All hot-path reads and writes go through lock-free DashMap operations.
// Persistence (Tantivy indexing, Git commits) is async via crossbeam channel.
//
// ARCHITECTURAL NOTE: DashMap state (agents, inboxes, projects) is ephemeral
// and lost on restart. Tantivy index persists on disk. After restart,
// search_messages returns historical data but agents/inboxes are empty.
// This is by design: the hot path prioritizes speed over durability.
// Clients must re-register agents after a server restart.
#[derive(Clone)]
pub struct PostOffice {
    // Lock-free concurrent state (microsecond access)
    pub projects: Arc<DashMap<String, String>>, // project_id → human_key
    pub agents: Arc<DashMap<String, AgentRecord>>, // agent_name → record
    pub inboxes: Arc<DashMap<String, Vec<InboxEntry>>>, // agent_name → unread messages

    // Tantivy search (read path only — writes go through persist pipeline)
    pub index: Arc<Index>,
    pub index_reader: IndexReader,
    pub search_fields: SearchFields, // resolved once at startup (DR37-H3)

    // Async persistence pipeline (fire-and-forget from hot path)
    pub persist_tx: CbSender<PersistOp>,

    // NRT shutdown guard — when the last PostOffice clone is dropped,
    // the guard sets a flag that stops the NRT reader refresh task.
    _nrt_guard: Arc<NrtShutdownGuard>,

    // Persist worker JoinHandle — stored so shutdown() can join the thread
    // and wait for pending Tantivy batches to commit (DR34-H3).
    persist_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    // Git actor JoinHandle — stored so shutdown() can join the thread
    // after the persist worker exits (which drops the last git_tx sender,
    // closing the Git channel). Ensures pending git commits complete
    // before process exit (DR35-H1).
    git_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl PostOffice {
    pub fn new(index_path: &Path, repo_root: &Path) -> anyhow::Result<Self> {
        // ── 1. Tantivy Search Index ──────────────────────────────────────────
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("project_id", STRING | STORED);
        schema_builder.add_text_field("from_agent", STRING | STORED);
        // TEXT (not STRING) so the joined recipient list is tokenized,
        // enabling per-recipient search in multi-recipient messages.
        // The \x1F separator is stripped during tokenization (DR36-H5).
        schema_builder.add_text_field("to_recipients", TEXT | STORED);
        schema_builder.add_text_field("subject", TEXT | STORED);
        schema_builder.add_text_field("body", TEXT | STORED);
        schema_builder.add_i64_field("created_ts", INDEXED | STORED);
        schema_builder.add_text_field("id", STRING | STORED);
        let schema = schema_builder.build();

        // Resolve field handles once at startup (DR37-H3).
        let search_fields = SearchFields {
            id: schema.get_field("id").unwrap(),
            project_id: schema.get_field("project_id").unwrap(),
            from_agent: schema.get_field("from_agent").unwrap(),
            to_recipients: schema.get_field("to_recipients").unwrap(),
            subject: schema.get_field("subject").unwrap(),
            body: schema.get_field("body").unwrap(),
            created_ts: schema.get_field("created_ts").unwrap(),
        };

        std::fs::create_dir_all(index_path).context("Failed to create index directory")?;

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(index_path)?,
            schema.clone(),
        )
        .context("Failed to open tantivy index")?;

        let index_writer = index.writer(50_000_000)?;
        let index_reader = index.reader()?;

        // ── 2. Git Actor (dedicated OS thread — git2 is sync) ────────────────
        // JoinHandle stored so shutdown() can join after persist worker exits,
        // ensuring pending git commits complete before process exit (DR35-H1).
        let (git_tx, git_rx) = bounded::<GitRequest>(10_000);
        let actor = GitActor::new(repo_root.to_path_buf(), git_rx);
        let git_handle = std::thread::Builder::new()
            .name("git-actor".into())
            .spawn(move || actor.run())
            .context("Failed to spawn git actor thread")?;

        // ── 3. Persistence Worker (dedicated OS thread) ──────────────────────
        // Owns the Tantivy IndexWriter exclusively. Batches operations for throughput.
        // Pattern: block on first op → drain pending → commit batch.
        // JoinHandle stored so shutdown() can wait for pending batches (DR34-H3).
        let (persist_tx, persist_rx) = bounded::<PersistOp>(100_000);
        let persist_handle = {
            let git_tx = git_tx.clone();
            let sf = search_fields;
            std::thread::Builder::new()
                .name("persist-worker".into())
                .spawn(move || {
                    persistence_worker(persist_rx, index_writer, sf, git_tx);
                })
                .context("Failed to spawn persistence worker")?
        };

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
                if shutdown_flag.load(Ordering::Acquire) {
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
            search_fields,
            index: Arc::new(index),
            index_reader,
            persist_tx,
            _nrt_guard: nrt_guard,
            persist_handle: Arc::new(Mutex::new(Some(persist_handle))),
            git_handle: Arc::new(Mutex::new(Some(git_handle))),
        })
    }

    /// Shut down the persist pipeline and wait for all background threads
    /// to complete. Sequence:
    /// 1. Drop persist_tx → persist worker drains remaining ops and exits.
    /// 2. Join persist worker → all Tantivy batches committed (DR34-H3).
    /// 3. Persist worker exit drops git_tx → Git channel closes.
    /// 4. Join git actor → all pending git commits complete (DR35-H1).
    pub fn shutdown(self) {
        // Drop the sender to close the channel — this signals the persist
        // worker to drain remaining ops and exit its loop.
        drop(self.persist_tx);

        // Join the persist worker thread so pending batches are committed.
        // When the persist worker exits, it drops its git_tx clone (the last
        // sender), which closes the Git channel.
        // Recover from poisoned mutex to avoid silently skipping the join
        // and losing pending data (DR38-H1).
        let mut guard = self
            .persist_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.take() {
            if let Err(e) = handle.join() {
                tracing::error!("Persist worker thread panicked: {:?}", e);
            }
        }
        drop(guard);

        // Join the git actor thread so pending git commits complete.
        // The Git channel is now closed (persist worker dropped the last
        // sender above), so the git actor drains remaining requests and
        // exits. Joining ensures all commits finish before process exit
        // (DR35-H1, DR38-H1).
        let mut guard = self.git_handle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.take() {
            if let Err(e) = handle.join() {
                tracing::error!("Git actor thread panicked: {:?}", e);
            }
        }
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
    sf: SearchFields,
    git_tx: CbSender<GitRequest>,
) {
    tracing::info!("Persistence worker started");

    // Use the same pre-resolved field handles as search_messages,
    // eliminating the fragile coupling-by-convention where both sides
    // independently resolved handles from separate schema clones (DR38-H2).
    let f_id = sf.id;
    let f_project = sf.project_id;
    let f_from = sf.from_agent;
    let f_to = sf.to_recipients;
    let f_subject = sf.subject;
    let f_body = sf.body;
    let f_ts = sf.created_ts;

    // Allocate batch buffer once outside the loop. Reused across iterations
    // via clear() to avoid ~200KB heap allocation per batch (DR40-H2).
    let mut batch: Vec<PersistOp> = Vec::with_capacity(4096);

    loop {
        // Block until the first operation arrives
        let first = match rx.recv() {
            Ok(op) => op,
            Err(_) => {
                tracing::info!("Persistence worker shutting down (channel closed)");
                break;
            }
        };

        // Drain all pending ops (non-blocking) up to batch limit.
        // clear() reuses the existing heap allocation (DR40-H2).
        batch.clear();
        batch.push(first);
        while batch.len() < 4096 {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }

        let mut indexed = 0usize;

        for op in batch.drain(..) {
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
                    // Only count successful additions — failed add_document
                    // calls should not trigger a commit or inflate the batch
                    // count in error logs (DR36-H1).
                    match index_writer.add_document(doc) {
                        Ok(_) => indexed += 1,
                        Err(e) => {
                            tracing::error!("Tantivy add_document failed: {}", e)
                        }
                    }
                }
                PersistOp::GitCommit {
                    path,
                    content,
                    message,
                } => {
                    // Move path by value into the request — no clone needed
                    // on the success path. On failure, extract path from the
                    // error's returned value for the log message (DR40-H3).
                    let req = GitRequest::CommitFile {
                        path,
                        content,
                        message,
                    };
                    if let Err(e) = git_tx.try_send(req) {
                        tracing::warn!("Git channel full, dropping commit: {}", e);
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
