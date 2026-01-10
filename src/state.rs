use crate::git_actor::{GitActor, GitRequest};
use anyhow::Context;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tantivy::{schema::*, Index, IndexReader, IndexWriter};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct PostOffice {
    pub db: SqlitePool,
    pub index: Arc<Index>,
    pub index_writer: Arc<Mutex<IndexWriter>>,
    pub index_reader: IndexReader,
    pub git_sender: mpsc::Sender<GitRequest>,
}

impl PostOffice {
    pub async fn new(
        database_url: &str,
        index_path: &Path,
        repo_root: &Path,
    ) -> anyhow::Result<Self> {
        // 1. Initialize SQLite
        let db = SqlitePoolOptions::new()
            .max_connections(50)
            .connect(database_url)
            .await
            .context("Failed to connect to database")?;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .context("Failed to run migrations")?;

        // 2. Initialize Tantivy
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("project_id", STRING | STORED);
        schema_builder.add_text_field("from_agent", STRING | STORED);
        schema_builder.add_text_field("to_recipients", STRING | STORED); // New field
        schema_builder.add_text_field("subject", TEXT | STORED);
        schema_builder.add_text_field("body", TEXT | STORED); // Stored for snippets
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
        let writer_mutex = Arc::new(Mutex::new(index_writer));

        // Spawn Background Committer (Batching)
        let writer_clone = writer_mutex.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut writer = writer_clone.lock().unwrap();
                if let Err(e) = writer.commit() {
                     eprintln!("Error committing index: {}", e);
                }
            }
        });

        // Spawn NRT Refresher
        let reader_clone = index_reader.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Err(e) = reader_clone.reload() {
                    eprintln!("Error reloading index reader: {}", e);
                }
            }
        });

        // 3. Initialize Git Actor
        let (tx, rx) = mpsc::channel(100);
        let actor = GitActor::new(repo_root.to_path_buf(), rx);
        tokio::spawn(actor.run());

        Ok(PostOffice {
            db,
            index: Arc::new(index),
            index_writer: writer_mutex,
            index_reader,
            git_sender: tx,
        })
    }
}
