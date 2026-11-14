-- The session tables ZCode 3.18.2 creates in ~/.zcode/cli/db/db.sqlite (apps/zcode-cli/packages/adapters/src/storage/session-store/schema.baseline.sql in zai-org/ZCode)

CREATE TABLE message (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      , sequence integer);

CREATE TABLE part (
        id text primary key,
        message_id text not null references message(id) on delete cascade,
        session_id text not null,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      , sequence integer);

CREATE TABLE session (
        id text primary key,
        project_id text not null,
        workspace_id text,
        parent_id text,
        slug text not null,
        directory text not null,
        path text,
        title text not null,
        version text not null,
        share_url text,
        summary_additions integer,
        summary_deletions integer,
        summary_files integer,
        summary_diffs text,
        revert text,
        permission text,
        time_created integer not null,
        time_updated integer not null,
        time_compacting integer,
        time_archived integer
      , task_type text not null default 'interactive', title_source text not null default 'first_input'
        check(title_source in ('default', 'first_input', 'generated', 'custom')), title_message_id text, time_title_updated integer, trace_id text, time_deleted integer, last_outcome text);

CREATE TABLE session_entry (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        type text not null,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      );

CREATE TABLE session_purge (
        session_id text primary key,
        root_id text not null,
        operation_id text not null,
        retained_artifacts text not null,
        workspace_path text not null,
        workspace_identity text,
        state text not null check(state in ('deleting', 'deleted'))
      );

CREATE INDEX message_session_sequence_idx
        on message(session_id, sequence, time_created, id);

CREATE INDEX message_session_time_created_id_idx
        on message(session_id, time_created, id);

CREATE INDEX part_message_id_id_idx on part(message_id, id);

CREATE INDEX part_message_sequence_idx
        on part(message_id, sequence, time_created, id);

CREATE INDEX part_session_idx on part(session_id);

CREATE INDEX part_session_message_sequence_idx
        on part(session_id, message_id, sequence);

CREATE INDEX session_entry_session_idx on session_entry(session_id);

CREATE INDEX session_entry_session_type_idx on session_entry(session_id, type);

CREATE INDEX session_entry_time_created_idx on session_entry(time_created);

CREATE INDEX session_parent_idx on session(parent_id);

CREATE INDEX session_project_idx on session(project_id);

CREATE INDEX session_purge_root_idx on session_purge(root_id);

CREATE INDEX session_task_type_idx on session(task_type);

CREATE INDEX session_trace_idx on session(trace_id);

CREATE INDEX session_workspace_idx on session(workspace_id);

CREATE TRIGGER message_sequence_autofill
      after insert on message
      when new.sequence is null
      begin
        update message
        set sequence = (
          select coalesce(max(sequence), -1) + 1
          from message
          where session_id = new.session_id
        )
        where id = new.id;
      end;

CREATE TRIGGER part_sequence_autofill
      after insert on part
      when new.sequence is null
      begin
        update part
        set sequence = (
          select coalesce(max(sequence), -1) + 1
          from part
          where message_id = new.message_id
        )
        where id = new.id;
      end;

