-- The tables of OpenCode 2.x's database the source reads, as OpenCode 2.3.0
-- creates them. A fixture of a 2.x session runs this first; one of a
-- database 2.x migrated runs schema.sql before it, which made event_sequence
-- already, as 1.x did.
CREATE TABLE `session_v2` (
	`id` text PRIMARY KEY,
	`project_id` text NOT NULL,
	`workspace_id` text,
	`parent_id` text,
	`fork_session_id` text,
	`fork_boundary` text,
	`slug` text NOT NULL,
	`directory` text NOT NULL,
	`path` text,
	`title` text,
	`version` text NOT NULL,
	`share_url` text,
	`summary_additions` integer,
	`summary_deletions` integer,
	`summary_files` integer,
	`summary_diffs` text,
	`metadata` text,
	`cost` real DEFAULT 0 NOT NULL,
	`tokens_input` integer DEFAULT 0 NOT NULL,
	`tokens_output` integer DEFAULT 0 NOT NULL,
	`tokens_reasoning` integer DEFAULT 0 NOT NULL,
	`tokens_cache_read` integer DEFAULT 0 NOT NULL,
	`tokens_cache_write` integer DEFAULT 0 NOT NULL,
	`revert` text,
	`permission` text,
	`agent` text,
	`model` text,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`time_busy` integer,
	`time_idle` integer,
	`time_viewed` integer,
	`idle_outcome` text,
	`time_compacting` integer,
	`time_archived` integer,
	`time_suspended` integer,
	`resume_attempts` integer DEFAULT 0 NOT NULL,
	CONSTRAINT `fk_session_v2_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE INDEX `session_v2_project_idx` ON `session_v2` (`project_id`);
CREATE INDEX `session_v2_workspace_idx` ON `session_v2` (`workspace_id`);
CREATE INDEX `session_v2_parent_idx` ON `session_v2` (`parent_id`);
CREATE INDEX `session_v2_time_suspended_idx` ON `session_v2` (`time_suspended`) WHERE "session_v2"."time_suspended" is not null;
CREATE TABLE `session_message` (
	`id` text PRIMARY KEY,
	`session_id` text NOT NULL,
	`type` text NOT NULL,
	`seq` integer NOT NULL,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`data` text NOT NULL,
	CONSTRAINT `fk_session_message_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);
CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);
CREATE TABLE `session_part` (
	`message_id` text NOT NULL,
	`session_id` text NOT NULL,
	`ordinal` integer NOT NULL,
	`type` text NOT NULL,
	`call_id` text,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`data` text NOT NULL,
	CONSTRAINT `session_part_pk` PRIMARY KEY(`message_id`, `ordinal`),
	CONSTRAINT `fk_session_part_message_id_session_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `session_message`(`id`) ON DELETE CASCADE,
	CONSTRAINT `fk_session_part_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE INDEX `session_part_session_message_idx` ON `session_part` (`session_id`,`message_id`,`ordinal`);
CREATE TABLE `job` (
	`notification_id` text PRIMARY KEY,
	`id` text NOT NULL,
	`session_id` text NOT NULL,
	`child_session_id` text,
	`kind` text NOT NULL,
	`title` text,
	`status` text NOT NULL,
	`output` text,
	`error` text,
	`recovery` text NOT NULL,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL
);
CREATE INDEX `job_session_idx` ON `job` (`session_id`);
CREATE INDEX `job_child_session_idx` ON `job` (`child_session_id`);
CREATE INDEX `job_id_idx` ON `job` (`id`);
CREATE TABLE IF NOT EXISTS `event_sequence` (
	`aggregate_id` text PRIMARY KEY,
	`seq` integer NOT NULL,
	`owner_id` text
);
