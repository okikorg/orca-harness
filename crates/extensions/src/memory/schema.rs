pub(super) const SCHEMA_VERSION: i64 = 2;

pub(super) const RECORD_COLUMNS: &str = "
    m.public_id, m.content, m.kind, m.is_global, m.workspace_id,
    m.workspace_root, m.source_call_id, m.created_at, m.updated_at
";

pub(super) const MIGRATE_V1_TO_V2: &str = r#"
ALTER TABLE memories ADD COLUMN public_id TEXT;
UPDATE memories
SET public_id = 'mem_' || lower(hex(randomblob(16)))
WHERE public_id IS NULL;
CREATE UNIQUE INDEX memories_public_id ON memories(public_id);
PRAGMA user_version = 2;
"#;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE memories (
    id INTEGER PRIMARY KEY,
    public_id TEXT NOT NULL UNIQUE
        CHECK (length(public_id) = 36 AND substr(public_id, 1, 4) = 'mem_'),
    content TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_global INTEGER NOT NULL CHECK (is_global IN (0, 1)),
    workspace_id TEXT,
    workspace_root TEXT NOT NULL,
    source_call_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((is_global = 1 AND workspace_id IS NULL) OR
           (is_global = 0 AND workspace_id IS NOT NULL))
);
CREATE INDEX memories_scope ON memories(is_global, workspace_id, updated_at DESC);
CREATE VIRTUAL TABLE memories_fts USING fts5(
    content,
    kind,
    content='memories',
    content_rowid='id',
    tokenize='unicode61'
);
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, kind)
    VALUES (new.id, new.content, new.kind);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, kind)
    VALUES ('delete', old.id, old.content, old.kind);
END;
CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, kind)
    VALUES ('delete', old.id, old.content, old.kind);
    INSERT INTO memories_fts(rowid, content, kind)
    VALUES (new.id, new.content, new.kind);
END;
PRAGMA user_version = 2;
"#;
