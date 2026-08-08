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
        M::up(
            // S2-T2: which extra tool profile a session ran with (audit + resume).
            "ALTER TABLE sessions ADD COLUMN tools_profile TEXT;",
        ),
        M::up(
            // Этап 3: the GitHub daemon's task queue. `key` is the idempotency
            // anchor (issue:{slug}#{n} / pr-comment:{slug}#{pr}:{comment}) — a
            // restart can re-discover the same event without duplicating work.
            r#"
        CREATE TABLE daemon_tasks (
            key         TEXT PRIMARY KEY,
            kind        TEXT NOT NULL,
            state       TEXT NOT NULL,
            title       TEXT NOT NULL,
            payload     TEXT NOT NULL,
            branch      TEXT,
            session_id  TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        "#,
        ),
        M::up(
            // The frontend's transcript (text/thinking/tool calls/status — everything
            // rendered in the chat view) only ever lived in memory; a restart lost it
            // even for a session that finished cleanly. `items_json` is the frontend's
            // own serialized TranscriptItem[], so there is nothing to reparse or
            // re-derive on the way back out.
            r#"
        CREATE TABLE session_transcripts (
            session_id  TEXT PRIMARY KEY REFERENCES sessions(id),
            items_json  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        "#,
        ),
    ])
}
