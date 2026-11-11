-- The conversation tables of RikkaHub's rikka_hub.db at schema version 32 (app/schemas/me.rerere.rikkahub.data.db.AppDatabase/32.json in rikkahub/rikkahub 90269e3), with the identity Room checks when it opens the database
PRAGMA user_version = 32;
CREATE TABLE IF NOT EXISTS `ConversationEntity` (`id` TEXT NOT NULL, `assistant_id` TEXT NOT NULL DEFAULT '0950e2dc-9bd5-4801-afa3-aa887aa36b4e', `title` TEXT NOT NULL, `nodes` TEXT NOT NULL, `create_at` INTEGER NOT NULL, `update_at` INTEGER NOT NULL, `suggestions` TEXT NOT NULL DEFAULT '[]', `is_pinned` INTEGER NOT NULL DEFAULT 0, `custom_system_prompt` TEXT NOT NULL DEFAULT '', `mode_injection_ids` TEXT NOT NULL DEFAULT '[]', `lorebook_ids` TEXT NOT NULL DEFAULT '[]', `workspace_cwd` TEXT NOT NULL DEFAULT '', `folder_id` TEXT NOT NULL DEFAULT '', `config` TEXT NOT NULL DEFAULT '', PRIMARY KEY(`id`));
CREATE TABLE IF NOT EXISTS `memory_consolidation` (`conversation_id` TEXT NOT NULL, `consolidated_at` INTEGER NOT NULL, PRIMARY KEY(`conversation_id`), FOREIGN KEY(`conversation_id`) REFERENCES `ConversationEntity`(`id`) ON UPDATE NO ACTION ON DELETE CASCADE );
CREATE TABLE IF NOT EXISTS `message_node` (`id` TEXT NOT NULL, `conversation_id` TEXT NOT NULL, `node_index` INTEGER NOT NULL, `messages` TEXT NOT NULL, `select_index` INTEGER NOT NULL, PRIMARY KEY(`id`), FOREIGN KEY(`conversation_id`) REFERENCES `ConversationEntity`(`id`) ON UPDATE NO ACTION ON DELETE CASCADE );
CREATE INDEX IF NOT EXISTS `index_message_node_conversation_id` ON `message_node` (`conversation_id`);
CREATE TABLE IF NOT EXISTS `managed_files` (`id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, `folder` TEXT NOT NULL, `relative_path` TEXT NOT NULL, `display_name` TEXT NOT NULL, `mime_type` TEXT NOT NULL, `size_bytes` INTEGER NOT NULL, `created_at` INTEGER NOT NULL, `updated_at` INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS `index_managed_files_relative_path` ON `managed_files` (`relative_path`);
CREATE INDEX IF NOT EXISTS `index_managed_files_folder` ON `managed_files` (`folder`);
CREATE TABLE IF NOT EXISTS `favorites` (`id` TEXT NOT NULL, `type` TEXT NOT NULL, `ref_key` TEXT NOT NULL, `ref_json` TEXT NOT NULL, `snapshot_json` TEXT NOT NULL, `meta_json` TEXT, `created_at` INTEGER NOT NULL, `updated_at` INTEGER NOT NULL, PRIMARY KEY(`id`));
CREATE UNIQUE INDEX IF NOT EXISTS `index_favorites_ref_key` ON `favorites` (`ref_key`);
CREATE INDEX IF NOT EXISTS `index_favorites_type` ON `favorites` (`type`);
CREATE INDEX IF NOT EXISTS `index_favorites_created_at` ON `favorites` (`created_at`);
CREATE TABLE IF NOT EXISTS room_master_table (id INTEGER PRIMARY KEY,identity_hash TEXT);
INSERT OR REPLACE INTO room_master_table (id,identity_hash) VALUES(42, 'c64deeb13766eaf6528a2f4f6965c836');
