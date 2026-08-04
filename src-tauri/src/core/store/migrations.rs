//! Schema migrations. Append-only: never edit an existing migration, add a new one.

use rusqlite_migration::{Migrations, M};

pub fn runner() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            r#"
        CREATE TABLE branches (
            name        TEXT PRIMARY KEY,
            task_id     TEXT,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE sessions (
            id          TEXT PRIMARY KEY,
            branch      TEXT NOT NULL REFERENCES branches(name),
            type        TEXT NOT NULL,
            status      TEXT NOT NULL,
            model       TEXT,
            effort      TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE INDEX idx_sessions_branch ON sessions(branch);
        "#,
        ),
        M::up(
            // T2: branches remember their base branch; key-value settings for app state
            // (selected repo path, branch naming convention).
            r#"
        ALTER TABLE branches ADD COLUMN base_branch TEXT;

        CREATE TABLE settings (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        );
        "#,
        ),
        M::up(
            // T3: persist the Claude Agent SDK session id for resume support.
            "ALTER TABLE sessions ADD COLUMN sdk_session_id TEXT;",
        ),
        M::up(
            // T4: persist the permission mode (single-writer rule, read-only badge).
            "ALTER TABLE sessions ADD COLUMN permission_mode TEXT;",
        ),
        M::up(
            // S3: persist the thinking budget, so a resumed session thinks as much as
            // the one it continues.
            "ALTER TABLE sessions ADD COLUMN thinking TEXT;",
        ),
    ])
}
