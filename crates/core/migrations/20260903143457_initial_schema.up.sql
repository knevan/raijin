CREATE TABLE downloads (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    url TEXT NOT NULL,
    download_page TEXT,
    headers_json TEXT,
    file_name TEXT NOT NULL,
    folder TEXT NOT NULL,
    status TEXT NOT NULL,
    total_bytes INTEGER,
    downloaded_bytes INTEGER NOT NULL DEFAULT 0,
    etag TEXT,
    last_modified TEXT,
    preferred_connections INTEGER,
    speed_limit_bps INTEGER,
    error_kind TEXT,
    error_message TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    completed_at INTEGER,
    updated_at INTEGER NOT NULL,
    CHECK (length(kind) > 0),
    CHECK (length(url) > 0),
    CHECK (length(file_name) > 0),
    CHECK (length(folder) > 0),
    CHECK (length(status) > 0),
    CHECK (total_bytes IS NULL OR total_bytes >= 0),
    CHECK (downloaded_bytes >= 0),
    CHECK (total_bytes IS NULL OR downloaded_bytes <= total_bytes),
    CHECK (preferred_connections IS NULL OR preferred_connections > 0),
    CHECK (speed_limit_bps IS NULL OR speed_limit_bps >= 0),
    CHECK (created_at >= 0),
    CHECK (started_at IS NULL OR started_at >= 0),
    CHECK (completed_at IS NULL OR completed_at >= 0),
    CHECK (updated_at >= 0)
);

CREATE TABLE download_parts (
    id INTEGER PRIMARY KEY,
    download_id INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    part_index INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER,
    current_byte INTEGER NOT NULL,
    status TEXT NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    UNIQUE (download_id, part_index),
    CHECK (part_index >= 0),
    CHECK (start_byte >= 0),
    CHECK (end_byte IS NULL OR end_byte >= start_byte),
    CHECK (current_byte >= start_byte),
    CHECK (end_byte IS NULL OR current_byte <= end_byte + 1),
    CHECK (length(status) > 0),
    CHECK (retry_count >= 0),
    CHECK (updated_at >= 0)
);

CREATE TABLE queues (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    max_concurrent INTEGER NOT NULL DEFAULT 2,
    stop_on_empty INTEGER NOT NULL DEFAULT 0,
    schedule_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (length(name) > 0),
    CHECK (max_concurrent > 0),
    CHECK (stop_on_empty IN (0, 1)),
    CHECK (created_at >= 0),
    CHECK (updated_at >= 0)
);

CREATE TABLE queue_items (
    queue_id INTEGER NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
    download_id INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (queue_id, download_id),
    CHECK (position >= 0)
) WITHOUT ROWID;

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (length(key) > 0),
    CHECK (updated_at >= 0)
) WITHOUT ROWID;

CREATE INDEX idx_downloads_status ON downloads(status);
CREATE INDEX idx_queue_items_queue_position ON queue_items(queue_id, position);
