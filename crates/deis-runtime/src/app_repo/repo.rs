//! The per-application file store the module loader reads from.
//!
//! An application's entry module comes straight out of its bundle, but a
//! module it imports is resolved lazily, while the isolate is running, and the
//! zip is long closed by then. This table is where such a module would be
//! looked up (see `crate::runtime::loader`).
//!
//! There is deliberately no package format here. The bundle format has exactly
//! one definition, in `deis-bundle`, and one way into a node: `ApplyDeployment`
//! (docs/implementation-plan.zh.md section 4). A second manifest struct in this
//! crate is what let a real deislet bundle be rejected by one half of the node
//! and accepted by the other, so it is gone along with the upload path that
//! parsed it.
//!
//! What that leaves is a store with no writer: nothing populates `app_files`
//! today, so an import that is not the entry module fails to resolve. Wiring
//! the deployment path into this table (or removing it together with the
//! loader branch that reads it) is follow-up work that spans the runtime
//! module.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;

pub struct AppRepo {
    db_path: String,
}

impl AppRepo {
    pub async fn new(db_path: &str) -> Result<Self> {
        info!("Initializing AppRepo: {}", db_path);

        if let Some(parent) = std::path::Path::new(db_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let db_path_owned = db_path.to_string();
        let db_path_clone = db_path_owned.clone();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path_clone)?;
            Self::init_schema(&conn)?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        Ok(Self {
            db_path: db_path_owned,
        })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS app_files (
                app_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                content BLOB NOT NULL,
                PRIMARY KEY (app_id, file_path)
            )
            "#,
            [],
        )?;

        Ok(())
    }

    /// One file of one application, or `None` when this node does not hold it.
    ///
    /// `app_id` is the key the runtime pool uses, which is the tenant spelling
    /// of an (application, environment) pair - two environments of one
    /// application do not share module files any more than they share secrets.
    pub async fn get_file(&self, app_id: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let db_path = self.db_path.clone();
        let id = app_id.to_string();
        let path = path.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt =
                conn.prepare("SELECT content FROM app_files WHERE app_id = ?1 AND file_path = ?2")?;
            let content: Option<Vec<u8>> = stmt
                .query_row(params![id, path], |row| row.get(0))
                .optional()?;
            Ok(content)
        })
        .await?
    }
}
