-- The session tables Devin CLI 3000.6.7 creates in sessions.db (schema 16), plus the search trigger of schema 19
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  working_directory TEXT NOT NULL,
  backend_type TEXT NOT NULL,
  model TEXT NOT NULL,
  agent_mode TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL, title TEXT, main_chain_id INTEGER, shell_last_seen_index INTEGER DEFAULT 0, cogs_json TEXT, workspace_dirs TEXT, hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT);
CREATE TABLE message_nodes (
  row_id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  node_id INTEGER NOT NULL,
  parent_node_id INTEGER,
  chat_message TEXT NOT NULL,
  created_at INTEGER NOT NULL, metadata TEXT,
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE(session_id, node_id)
);
CREATE VIRTUAL TABLE message_search USING fts5(content, content = '', contentless_delete = 1, tokenize = 'unicode61');
CREATE TRIGGER message_search_after_insert
AFTER INSERT ON message_nodes
WHEN CASE WHEN json_valid(new.chat_message) THEN
  (json_extract(new.chat_message, '$.role') = 'assistant'
   OR (json_extract(new.chat_message, '$.role') = 'user'
       AND json_extract(new.chat_message, '$.metadata.is_user_input') = 1))
  AND typeof(json_extract(new.chat_message, '$.content')) = 'text'
  AND json_extract(new.chat_message, '$.content') <> ''
ELSE 0 END
BEGIN
  INSERT INTO message_search (rowid, content)
  VALUES (new.row_id, json_extract(new.chat_message, '$.content'));
END;
