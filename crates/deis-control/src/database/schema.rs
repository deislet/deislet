use rusqlite::{Connection, Result};
use tracing::info;

/// Initialize database schema
pub fn create_tables(conn: &Connection) -> Result<()> {
    info!("Creating database tables...");

    // Enable foreign key constraints
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    // Workers table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS workers (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL UNIQUE,
            worker_type TEXT NOT NULL,
            version TEXT NOT NULL,
            host TEXT NOT NULL,
            ports TEXT NOT NULL,
            state TEXT NOT NULL,
            connected_at TEXT NOT NULL,
            last_heartbeat TEXT NOT NULL,
            power_reserve INTEGER DEFAULT 0,
            metadata TEXT
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_workers_state ON workers(state)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_workers_last_heartbeat ON workers(last_heartbeat)",
        [],
    )?;

    // Configs table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS configs (
            worker_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (worker_id, config_key)
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_configs_version ON configs(version_timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_configs_worker ON configs(worker_id)",
        [],
    )?;

    // Config history table (for auditing and rollback)
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS config_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            worker_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            operation TEXT NOT NULL,
            created_at TEXT NOT NULL
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_worker ON config_history(worker_id, created_at DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_key ON config_history(worker_id, config_key, created_at DESC)",
        [],
    )?;

    // Heartbeats table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS heartbeats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            worker_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            power_reserve INTEGER NOT NULL,
            latency_ms INTEGER,
            FOREIGN KEY (worker_id) REFERENCES workers(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_worker ON heartbeats(worker_id, timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_timestamp ON heartbeats(timestamp)",
        [],
    )?;

    // Commands table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS commands (
            id TEXT PRIMARY KEY,
            worker_id TEXT NOT NULL,
            command_type TEXT NOT NULL,
            command_data TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            sent_at TEXT,
            completed_at TEXT,
            timeout_at TEXT,
            response_data TEXT,
            error_message TEXT,
            FOREIGN KEY (worker_id) REFERENCES workers(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_worker ON commands(worker_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_status ON commands(status)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_created ON commands(created_at)",
        [],
    )?;

    // Events table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            worker_id TEXT,
            severity TEXT NOT NULL,
            message TEXT NOT NULL,
            metadata TEXT,
            timestamp TEXT NOT NULL,
            FOREIGN KEY (worker_id) REFERENCES workers(id) ON DELETE SET NULL
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_worker ON events(worker_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_severity ON events(severity)",
        [],
    )?;

    // Cron tasks table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS cron_tasks (
            id TEXT PRIMARY KEY,
            worker_id TEXT NOT NULL,
            cron_expr TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            last_run TEXT,
            next_run TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (worker_id) REFERENCES workers(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_cron_tasks_worker ON cron_tasks(worker_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_cron_tasks_next_run ON cron_tasks(next_run)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_cron_tasks_enabled ON cron_tasks(enabled)",
        [],
    )?;

    info!("Database tables created successfully");
    Ok(())
}
