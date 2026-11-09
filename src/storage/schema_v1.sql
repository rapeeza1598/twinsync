CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    source_path BLOB,
    settings_json TEXT NOT NULL DEFAULT '{}',
    source_generation INTEGER NOT NULL DEFAULT 0,
    last_scan_complete INTEGER NOT NULL DEFAULT 0,
    last_scan_at INTEGER
);

CREATE TABLE destinations (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id),
    slot TEXT NOT NULL,
    label TEXT NOT NULL,
    volume_uuid TEXT,
    volume_name TEXT,
    fs_type TEXT,
    media_type TEXT NOT NULL DEFAULT 'unknown',
    bus_protocol TEXT,
    physical_id TEXT,
    identity_strong INTEGER NOT NULL DEFAULT 0,
    performance_profile TEXT NOT NULL DEFAULT 'auto',
    root_rel_path BLOB NOT NULL DEFAULT x'',
    last_mount_path BLOB,
    free_bytes INTEGER,
    free_checked_at INTEGER,
    last_sync_at INTEGER,
    last_verify_at INTEGER,
    planned_generation INTEGER NOT NULL DEFAULT -1,
    UNIQUE (project_id, slot)
);

CREATE TABLE source_entries (
    project_id TEXT NOT NULL REFERENCES projects(id),
    rel_path BLOB NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    hash BLOB,
    seen_generation INTEGER NOT NULL,
    PRIMARY KEY (project_id, rel_path)
);

CREATE TABLE destination_entries (
    destination_id TEXT NOT NULL REFERENCES destinations(id),
    rel_path BLOB NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    synced_hash BLOB,
    dest_size INTEGER,
    dest_mtime_ns INTEGER,
    dest_inode INTEGER,
    src_size INTEGER,
    src_mtime_ns INTEGER,
    src_inode INTEGER,
    synced_at INTEGER,
    verified_at INTEGER,
    PRIMARY KEY (destination_id, rel_path)
);

CREATE TABLE sync_operations (
    id TEXT PRIMARY KEY,
    destination_id TEXT NOT NULL REFERENCES destinations(id),
    kind TEXT NOT NULL,
    rel_path BLOB NOT NULL,
    state TEXT NOT NULL,
    step TEXT NOT NULL,
    temp_name TEXT NOT NULL,
    version_rel_path BLOB,
    expected_old_hash BLOB,
    new_hash BLOB,
    src_size INTEGER,
    src_mtime_ns INTEGER,
    src_inode INTEGER,
    bytes_total INTEGER NOT NULL DEFAULT 0,
    bytes_done INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_ops_open ON sync_operations (destination_id, state);

CREATE TABLE file_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    destination_id TEXT NOT NULL REFERENCES destinations(id),
    rel_path BLOB NOT NULL,
    version_rel_path BLOB NOT NULL,
    size INTEGER NOT NULL,
    hash BLOB,
    op_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE activity_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    destination_id TEXT,
    kind TEXT NOT NULL,
    rel_path BLOB,
    result TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    retryable INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_activity_ts ON activity_events (ts DESC);
