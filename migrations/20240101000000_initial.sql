-- Initial Schema

CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    human_key TEXT,
    slug TEXT UNIQUE,
    created_ts INTEGER,
    meta TEXT -- JSON
);

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    project_id TEXT,
    name TEXT,
    program TEXT,
    model TEXT,
    inception_ts INTEGER,
    task TEXT,
    last_active_ts INTEGER,
    UNIQUE(project_id, name),
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    project_id TEXT,
    thread_id TEXT,
    subject TEXT,
    body_md TEXT,
    from_agent TEXT,
    created_ts INTEGER,
    importance TEXT,
    ack_required BOOLEAN,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS message_recipients (
    message_id TEXT,
    agent_name TEXT,
    kind TEXT, -- 'to' | 'cc' | 'bcc'
    read_ts INTEGER,
    ack_ts INTEGER,
    PRIMARY KEY(message_id, agent_name, kind),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS file_reservations (
    id TEXT PRIMARY KEY,
    project_id TEXT,
    agent_name TEXT,
    path TEXT,
    exclusive BOOLEAN,
    reason TEXT,
    created_ts INTEGER,
    expires_ts INTEGER,
    released_ts INTEGER,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
);
