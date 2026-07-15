//! Magic Context durable cache-state store.
//!
//! Persists the per-session `cortexkit-cache-core` [`CoreState`] plus a small
//! `module_meta` blob (`initialized`, `last_render_config`, `coverage_ordinal`).
//!
//! Concurrency: writes go through `cortexkit-store`'s epoch-fenced transaction
//! (rejects a superseded lease handover) AND an app-level `row_version` CAS inside
//! that same transaction. The epoch fence only rejects a STRICTLY-NEWER writer
//! (lease handover) — an equal-epoch writer is NOT fenced — so the row_version CAS
//! is what catches a same-epoch second writer. It is conditional: a pass writes
//! ONLY when durable state actually changed (a pure SoftPlus replay mutates
//! nothing and writes nothing), so the no-write-on-defer guarantee holds.

#![forbid(unsafe_code)]

use cortexkit_cache_core::CoreState;
use cortexkit_store::{open_sqlite, Migration, SqliteStore, StoreError};
use cortexkit_store_types::StorageDescriptor;
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};

pub type ProviderExtras = BTreeMap<String, BTreeMap<String, Value>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageOrigin {
    pub provider: String,
    pub model: String,
    pub api: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CkWireMessage {
    pub role: String,
    pub content: Vec<CkWireBlock>,
    pub origin: Option<MessageOrigin>,
    pub provider_extras: ProviderExtras,
    pub meta: HarnessMeta,
    /// Original parsed JSON for pass-through messages. Pass-through MUST stay
    /// Value-level: serializing this retained value, never a typed-struct round-trip,
    /// preserves harmless unknown fields and keeps replay lossless as the CK wire evolves.
    original: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CkWireMessageData {
    pub role: String,
    pub content: Vec<CkWireBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
    #[serde(default)]
    pub meta: HarnessMeta,
}

impl<'de> Deserialize<'de> for CkWireMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let original = Value::deserialize(deserializer)?;
        let data =
            CkWireMessageData::deserialize(original.clone()).map_err(serde::de::Error::custom)?;
        Ok(Self {
            role: data.role,
            content: data.content,
            origin: data.origin,
            provider_extras: data.provider_extras,
            meta: data.meta,
            original: Some(original),
        })
    }
}

impl Serialize for CkWireMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(original) = &self.original {
            return original.serialize(serializer);
        }
        CkWireMessageData {
            role: self.role.clone(),
            content: self.content.clone(),
            origin: self.origin.clone(),
            provider_extras: self.provider_extras.clone(),
            meta: self.meta.clone(),
        }
        .serialize(serializer)
    }
}

impl CkWireMessage {
    pub fn from_parts(
        role: impl Into<String>,
        content: Vec<CkWireBlock>,
        origin: Option<MessageOrigin>,
        provider_extras: ProviderExtras,
        meta: HarnessMeta,
    ) -> Self {
        Self {
            role: role.into(),
            content,
            origin,
            provider_extras,
            meta,
            original: None,
        }
    }

    pub fn synthetic_user_text(text: impl Into<String>) -> Self {
        Self::from_parts(
            "user",
            vec![CkWireBlock::bare(CkKind::Text { text: text.into() })],
            None,
            ProviderExtras::new(),
            HarnessMeta {
                synthetic: true,
                ..Default::default()
            },
        )
    }

    pub fn mark_modified(&mut self) {
        self.original = None;
    }

    fn mark_fully_typed(&mut self) {
        self.original = None;
        for block in &mut self.content {
            block.mark_modified();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CkWireBlock {
    pub kind: CkKind,
    pub provider_extras: ProviderExtras,
    /// Original parsed JSON for pass-through blocks. Keep this Value-level for the same
    /// lossless-pass-through reason as CkWireMessage::original.
    original: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CkWireBlockData {
    pub kind: CkKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

impl<'de> Deserialize<'de> for CkWireBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let original = Value::deserialize(deserializer)?;
        let data =
            CkWireBlockData::deserialize(original.clone()).map_err(serde::de::Error::custom)?;
        Ok(Self {
            kind: data.kind,
            provider_extras: data.provider_extras,
            original: Some(original),
        })
    }
}

impl Serialize for CkWireBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(original) = &self.original {
            return original.serialize(serializer);
        }
        CkWireBlockData {
            kind: self.kind.clone(),
            provider_extras: self.provider_extras.clone(),
        }
        .serialize(serializer)
    }
}

impl CkWireBlock {
    pub fn bare(kind: CkKind) -> Self {
        Self {
            kind,
            provider_extras: ProviderExtras::new(),
            original: None,
        }
    }

    pub fn with_provider_extras(kind: CkKind, provider_extras: ProviderExtras) -> Self {
        Self {
            kind,
            provider_extras,
            original: None,
        }
    }

    /// Drop the retained ingress bytes so serialization reflects an in-place
    /// mutation of the typed content. Every mutator that edits `kind` through a
    /// live block MUST call this: `Serialize` prefers `original` for lossless
    /// pass-through, so an uncleared block silently serializes its pre-mutation
    /// bytes and the edit never reaches the wire.
    pub fn mark_modified(&mut self) {
        self.original = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CkKind {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedReasoning {
        data: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        #[serde(default)]
        provider_executed: bool,
    },
    ToolResult {
        id: String,
        tool_name: String,
        output: CkToolOutput,
        #[serde(default)]
        provider_executed: bool,
    },
    Media(MediaBlock),
    Opaque(OpaqueBlock),
}

impl CkKind {
    pub fn tag(&self) -> &'static str {
        match self {
            CkKind::Text { .. } => "text",
            CkKind::Reasoning { .. } => "reasoning",
            CkKind::RedactedReasoning { .. } => "redacted_reasoning",
            CkKind::ToolCall { .. } => "tool_call",
            CkKind::ToolResult { .. } => "tool_result",
            CkKind::Media(_) => "media",
            CkKind::Opaque(_) => "opaque",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CkToolOutput {
    pub kind: CkOutputKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

impl CkToolOutput {
    pub fn bare(kind: CkOutputKind) -> Self {
        Self {
            kind,
            provider_extras: ProviderExtras::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CkOutputKind {
    Text { text: String },
    Json { value: Value },
    ErrorText { text: String },
    ErrorJson { value: Value },
    ExecutionDenied { reason: Option<String> },
    Content { blocks: Vec<ResultBlock> },
}

impl CkOutputKind {
    pub fn tag(&self) -> &'static str {
        match self {
            CkOutputKind::Text { .. } => "text",
            CkOutputKind::Json { .. } => "json",
            CkOutputKind::ErrorText { .. } => "error_text",
            CkOutputKind::ErrorJson { .. } => "error_json",
            CkOutputKind::ExecutionDenied { .. } => "execution_denied",
            CkOutputKind::Content { .. } => "content",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultBlock {
    pub kind: ResultBlockKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultBlockKind {
    Text { text: String },
    Media { media: MediaBlock },
    Opaque { opaque: OpaqueBlock },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueBlock {
    pub source: Value,
    pub kind: String,
    pub raw: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaBlock {
    pub kind: MediaKind,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub source: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    File,
    Document,
}

/// Migration namespace for the cache-state domain (one DB can host several
/// independent namespaces; this is ours).
const NS: &str = "mc_cache";

/// Durable namespace prefix for shadow-mode sessions and their mirror project rows.
pub const SHADOW_SESSION_PREFIX: &str = "shadow:";

/// Sentinel row_version meaning "no row present" (COALESCE default inside the txn).
const NO_ROW: i64 = -1;
const MAX_CHUNK_TRANSCRIPT_COMPRESSED_BYTES: usize = 256 * 1024;
const MAX_SESSION_TRANSCRIPT_COMPRESSED_BYTES: i64 = 8 * 1024 * 1024;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: "
        CREATE TABLE IF NOT EXISTS mc_cache_state (
            session_id   TEXT PRIMARY KEY,
            row_version  INTEGER NOT NULL,
            core_state   TEXT NOT NULL,
            meta         TEXT NOT NULL
        );
    ",
    },
    Migration {
        version: 2,
        // The compartment history (the m0/m1 render source). Keyed by
        // (session_id, sequence); sequence is the chronological order (1 = oldest).
        // `content` is the primary text (the P1 tier, or a legacy flat body); p1..p4
        // are the four paraphrase tiers a compartment can render at (NULL for legacy
        // rows); `importance` is the decay rate (1..100); `legacy=1` marks a pre-tier
        // flat row with no paraphrases.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_compartments (
            session_id        TEXT NOT NULL,
            sequence          INTEGER NOT NULL,
            start_message     INTEGER NOT NULL,
            end_message       INTEGER NOT NULL,
            start_message_id  TEXT NOT NULL DEFAULT '',
            end_message_id    TEXT NOT NULL DEFAULT '',
            title             TEXT NOT NULL,
            content           TEXT NOT NULL,
            p1                TEXT,
            p2                TEXT,
            p3                TEXT,
            p4                TEXT,
            importance        INTEGER NOT NULL DEFAULT 50,
            episode_type      TEXT,
            legacy            INTEGER NOT NULL DEFAULT 0,
            created_at        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, sequence)
        );
    ",
    },
    Migration {
        version: 3,
        // Project memories — the durable knowledge rendered into the prompt baseline.
        // Uses the full original `memories` schema (every field) so the background
        // maintenance worker that owns memory upkeep can read/write all of it; the
        // prompt render only projects the subset it needs. Keyed by id;
        // UNIQUE(project_path, category, normalized_hash) dedups. `importance` orders the
        // budget trim (highest survives); `status` selects active/permanent for the
        // render (archived is ignored); `superseded_by_memory_id` records that a later
        // memory replaced this one (used when rendering memory corrections).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_memories (
            id                       INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path             TEXT NOT NULL,
            category                 TEXT NOT NULL,
            content                  TEXT NOT NULL,
            normalized_hash          TEXT NOT NULL,
            importance               INTEGER,
            scope                    TEXT NOT NULL DEFAULT 'project',
            shareable                INTEGER NOT NULL DEFAULT 0,
            source_session_id        TEXT,
            source_type              TEXT DEFAULT 'historian',
            seen_count               INTEGER DEFAULT 1,
            retrieval_count          INTEGER DEFAULT 0,
            first_seen_at            INTEGER NOT NULL DEFAULT 0,
            created_at               INTEGER NOT NULL DEFAULT 0,
            updated_at               INTEGER NOT NULL DEFAULT 0,
            last_seen_at             INTEGER NOT NULL DEFAULT 0,
            last_retrieved_at        INTEGER,
            status                   TEXT DEFAULT 'active',
            expires_at               INTEGER,
            verification_status      TEXT DEFAULT 'unverified',
            verified_at              INTEGER,
            classified_at            INTEGER,
            superseded_by_memory_id  INTEGER,
            merged_from              TEXT,
            metadata_json            TEXT,
            UNIQUE(project_path, category, normalized_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_memories_project_status
            ON mc_memories(project_path, status);
    ",
    },
    Migration {
        version: 4,
        // Records non-additive memory changes (update / archive / delete / superseded)
        // as append-only rows instead of editing the rendered memory baseline. The
        // prompt baseline is cached byte-for-byte once rendered; rather than re-rendering
        // it for every memory edit (which would invalidate the cache), these rows are
        // coalesced to one correction per target memory and sent as a small "corrections"
        // delta on top of the cached baseline. On the next full baseline re-render the
        // corrections fold in and a cursor advances past the processed rows.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_memory_mutation_log (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path       TEXT NOT NULL,
            mutation_type      TEXT NOT NULL
                CHECK (mutation_type IN ('archive', 'delete', 'update', 'superseded')),
            target_memory_id   INTEGER NOT NULL,
            superseded_by_id   INTEGER,
            category           TEXT,
            new_content        TEXT,
            queued_at          INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_mc_memory_mutation_log_project
            ON mc_memory_mutation_log(project_path, id);
    ",
    },
    Migration {
        version: 5,
        // User memories — the <user-profile> baseline source. GLOBAL (cross-project, no
        // project_path): durable observations about the user that apply everywhere. The
        // render reads active ones ordered promoted_at ASC, id ASC (the id tiebreaker is
        // load-bearing: promoted_at can tie at ms granularity, and a non-deterministic
        // order would drift the rendered bytes between passes).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_user_memories (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            content              TEXT NOT NULL,
            status               TEXT NOT NULL DEFAULT 'active',
            promoted_at          INTEGER NOT NULL DEFAULT 0,
            source_candidate_ids TEXT DEFAULT '[]',
            created_at           INTEGER NOT NULL DEFAULT 0,
            updated_at           INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_mc_user_memories_status
            ON mc_user_memories(status);
    ",
    },
    Migration {
        version: 6,
        // Cross-project workspaces. A project belongs to at most one workspace (the
        // UNIQUE index on project_path). A member session reads the UNION of members'
        // memories, but a FOREIGN member's memories are visible only in the workspace's
        // shared categories (share_categories); the owning project always sees all its
        // own. share_categories is a JSON array (default ["CONSTRAINTS"]).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_workspaces (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            name             TEXT NOT NULL UNIQUE,
            created_at       INTEGER NOT NULL DEFAULT 0,
            updated_at       INTEGER NOT NULL DEFAULT 0,
            share_categories TEXT NOT NULL DEFAULT '[\"CONSTRAINTS\"]'
        );
        CREATE TABLE IF NOT EXISTS mc_workspace_members (
            workspace_id  INTEGER NOT NULL REFERENCES mc_workspaces(id) ON DELETE CASCADE,
            project_path  TEXT NOT NULL,
            display_name  TEXT NOT NULL,
            display_path  TEXT NOT NULL,
            added_at      INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (workspace_id, project_path)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_mc_workspace_member_unique
            ON mc_workspace_members(project_path);
    ",
    },
    Migration {
        version: 7,
        // Durable ctx_reduce arrival queue. Rows are removed only by the same fenced
        // transaction that commits the busting pass consuming them.
        statements: "
        CREATE TABLE IF NOT EXISTS pending_agent_drops (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            target_id   TEXT NOT NULL,
            queued_at   INTEGER NOT NULL DEFAULT 0,
            UNIQUE(session_id, target_id)
        );
        CREATE INDEX IF NOT EXISTS idx_pending_agent_drops_session
            ON pending_agent_drops(session_id, queued_at, id);
    ",
    },
    Migration {
        version: 8,
        // Shadow-mode mirrors live under the shadow key instead of the production
        // project-memory tables. This preserves source memory ids for byte-for-byte
        // comparison without colliding with real mc_memories rows that share the same ids.
        statements: "
        CREATE TABLE IF NOT EXISTS shadow_memories (
            shadow_project_path       TEXT NOT NULL,
            id                        INTEGER NOT NULL,
            category                  TEXT NOT NULL,
            content                   TEXT NOT NULL,
            normalized_hash           TEXT NOT NULL,
            importance                INTEGER,
            scope                     TEXT NOT NULL DEFAULT 'project',
            shareable                 INTEGER NOT NULL DEFAULT 0,
            source_session_id         TEXT,
            source_type               TEXT DEFAULT 'historian',
            seen_count                INTEGER DEFAULT 1,
            retrieval_count           INTEGER DEFAULT 0,
            first_seen_at             INTEGER NOT NULL DEFAULT 0,
            created_at                INTEGER NOT NULL DEFAULT 0,
            updated_at                INTEGER NOT NULL DEFAULT 0,
            last_seen_at              INTEGER NOT NULL DEFAULT 0,
            last_retrieved_at         INTEGER,
            status                    TEXT DEFAULT 'active',
            expires_at                INTEGER,
            verification_status       TEXT DEFAULT 'unverified',
            verified_at               INTEGER,
            classified_at             INTEGER,
            superseded_by_memory_id   INTEGER,
            merged_from               TEXT,
            metadata_json             TEXT,
            PRIMARY KEY (shadow_project_path, id)
        );
        CREATE INDEX IF NOT EXISTS idx_shadow_memories_project_status
            ON shadow_memories(shadow_project_path, status);

        CREATE TABLE IF NOT EXISTS shadow_memory_mutation_log (
            shadow_project_path  TEXT NOT NULL,
            id                   INTEGER NOT NULL,
            mutation_type        TEXT NOT NULL,
            target_memory_id     INTEGER NOT NULL,
            superseded_by_id     INTEGER,
            category             TEXT,
            new_content          TEXT,
            queued_at            INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (shadow_project_path, id)
        );
        CREATE INDEX IF NOT EXISTS idx_shadow_memory_mutation_project
            ON shadow_memory_mutation_log(shadow_project_path, id);

        CREATE TABLE IF NOT EXISTS shadow_divergences (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id           TEXT NOT NULL,
            pass_seq             INTEGER NOT NULL,
            class                TEXT NOT NULL,
            first_mid            TEXT,
            first_block          TEXT,
            first_field          TEXT,
            ts_prefix            TEXT NOT NULL,
            rs_prefix            TEXT NOT NULL,
            normalizations       TEXT NOT NULL,
            ts_decision          TEXT NOT NULL,
            rs_decision          TEXT NOT NULL,
            state_hash           TEXT NOT NULL,
            created_at           INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_shadow_divergences_session
            ON shadow_divergences(session_id, pass_seq, id);
    ",
    },
    Migration {
        version: 9,
        // Durable per-session receive/complete/reject timestamps and counts. Rejected
        // transforms still leave an audit trail here even when the cache-state table is
        // intentionally left unchanged.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_pass_trace (
            session_id             TEXT PRIMARY KEY,
            last_received_at_ms    INTEGER NOT NULL,
            last_completed_at_ms   INTEGER NOT NULL,
            last_reject_error      TEXT NULL,
            last_reject_at_ms      INTEGER NULL,
            reject_count           INTEGER NOT NULL DEFAULT 0,
            receive_count          INTEGER NOT NULL DEFAULT 0
        );
    ",
    },
    Migration {
        version: 10,
        // These transcript and session-note rows are append-only records. Transcript
        // rows are stored in the same publish transaction as the compartment rows, so a
        // failed publish cannot leave orphan transcripts and a crash after publish cannot
        // leave a compartment without its recoverable transcript.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_chunk_transcripts (
            session_id          TEXT NOT NULL,
            compartment_seq     INTEGER NOT NULL,
            start_ordinal       INTEGER NOT NULL,
            end_ordinal         INTEGER NOT NULL,
            transcript_deflate  BLOB NOT NULL,
            created_at_ms       INTEGER NOT NULL,
            PRIMARY KEY (session_id, compartment_seq)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_chunk_transcripts_session_range
            ON mc_chunk_transcripts(session_id, start_ordinal, end_ordinal, compartment_seq);

        CREATE TABLE IF NOT EXISTS mc_notes (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path       TEXT NOT NULL,
            session_id         TEXT NOT NULL,
            content            TEXT NOT NULL,
            status             TEXT NOT NULL CHECK (status IN ('active', 'dismissed')),
            surface_condition  TEXT NULL,
            anchor_block_id    TEXT NULL,
            created_at_ms      INTEGER NOT NULL,
            updated_at_ms      INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mc_notes_scope_status
            ON mc_notes(project_path, session_id, status, updated_at_ms DESC, id DESC);
    ",
    },
    Migration {
        version: 11,
        // U1 tagging surface. Tag rows are minted on first observation outside the
        // cache-state CAS path, so a rejected transform still preserves the monotonic
        // tag numbers the agent already saw. Channel-1 appends are an append-set keyed
        // by block id; replay reads the exact stored reminder bytes instead of deriving
        // them from mutable nudge state.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_tags (
            session_id     TEXT NOT NULL,
            tag_number    INTEGER NOT NULL,
            block_id      TEXT NOT NULL,
            kind          TEXT NOT NULL CHECK (kind IN ('message', 'tool_call', 'tool_result')),
            token_count   INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, tag_number),
            UNIQUE(session_id, block_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_tags_session_block
            ON mc_tags(session_id, block_id);

        CREATE TABLE IF NOT EXISTS mc_channel1_appends (
            session_id     TEXT NOT NULL,
            block_id       TEXT NOT NULL,
            reminder_text  TEXT NOT NULL,
            fired_at_ms    INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, block_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_channel1_appends_session
            ON mc_channel1_appends(session_id, fired_at_ms, block_id);
    ",
    },
    Migration {
        version: 12,
        // OpenCode shadow sync can carry pre-formatted compartment boundary dates.
        // Native historian rows leave these nullable until their harness provides a
        // canonical message-timestamp source.
        statements: "
        ALTER TABLE mc_compartments ADD COLUMN start_date TEXT;
        ALTER TABLE mc_compartments ADD COLUMN end_date TEXT;
    ",
    },
    Migration {
        version: 13,
        // Quarantine is represented by the divergence that caused it plus durable meta.
        // Older decision-only rows duplicate that terminal finding and carry no replay.
        statements: "
        DELETE FROM shadow_divergences WHERE class = 'quarantined';
    ",
    },
    Migration {
        version: 14,
        // Keep the original prefixes for readers that predate localized byte diagnostics.
        // The offset and centered windows make late mismatches directly inspectable.
        statements: "
        ALTER TABLE shadow_divergences ADD COLUMN first_diff_offset INTEGER;
        ALTER TABLE shadow_divergences ADD COLUMN ts_window TEXT NOT NULL DEFAULT '';
        ALTER TABLE shadow_divergences ADD COLUMN rs_window TEXT NOT NULL DEFAULT '';
    ",
    },
    Migration {
        version: 15,
        // Preserve the exact pre-overlay span at mint time. Existing rows predate this
        // provenance and remain explicitly empty; new rows never reconstruct source bytes
        // from forgeable tag syntax.
        statements: "
        ALTER TABLE mc_tags ADD COLUMN source_bytes BLOB NOT NULL DEFAULT X'';
    ",
    },
    Migration {
        version: 16,
        // Command ids survive queue consumption so a response-loss retry cannot reapply a
        // ctx_reduce request after its original drops have drained.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_reduce_command_ledger (
            session_id   TEXT NOT NULL,
            command_id   TEXT NOT NULL,
            queued_at_ms INTEGER NOT NULL,
            PRIMARY KEY (session_id, command_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_reduce_command_ledger_session_newest
            ON mc_reduce_command_ledger(session_id, queued_at_ms DESC, command_id DESC);
    ",
    },
    Migration {
        version: 17,
        // A completed import id is part of the session lineage. Keeping the acknowledgement
        // durable makes an outcome-unknown retry harmless even after the imported session has
        // advanced through later transform passes.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_state_imports (
            session_id      TEXT PRIMARY KEY,
            import_id       TEXT NOT NULL,
            imported_count  INTEGER NOT NULL,
            completed_at_ms INTEGER NOT NULL
        );
    ",
    },
    Migration {
        version: 18,
        // Auto-search decisions are append-only overlay bytes. Empty hint_text records a
        // durable no-result decision so changing memory state cannot rewrite an old turn.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_user_hints (
            session_id  TEXT NOT NULL,
            block_id    TEXT NOT NULL,
            hint_text   TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            PRIMARY KEY (session_id, block_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_user_hints_session_created
            ON mc_user_hints(session_id, created_at, block_id);
    ",
    },
];

/// A project's workspace membership: the union of member identities it reads, which of
/// them are its OWN (full visibility) vs FOREIGN (visible only in `share_categories`),
/// the shared-category allow-list, and per-foreign-member display attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMembership {
    /// All member project_paths (own + foreign), the union read set.
    pub union_identities: Vec<String>,
    /// The calling project's own identity (full-visibility).
    pub own_identity: String,
    /// Categories in which FOREIGN members' memories are shared into this project.
    pub share_categories: Vec<String>,
    /// project_path → display_name, for repo-attributing a foreign memory on render.
    pub display_name_by_path: std::collections::HashMap<String, String>,
}

/// A memory mutation-log entry. `update` is non-terminal (the memory is still present
/// with new content → renders `<updated>`); `archive`/`delete`/`superseded` are TERMINAL
/// (the memory left the active set → renders `<removed>`/`<superseded>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemoryMutation {
    pub id: i64,
    pub mutation_type: String,
    pub target_memory_id: i64,
    pub superseded_by_id: Option<i64>,
    pub category: Option<String>,
    pub new_content: Option<String>,
    pub queued_at: i64,
}

impl StoredMemoryMutation {
    fn is_terminal(&self) -> bool {
        matches!(
            self.mutation_type.as_str(),
            "archive" | "delete" | "superseded"
        )
    }
}

/// The durable historian single-flight phase. The phase lives in [`ModuleMeta`] so
/// the same row-version CAS that guards cache-state commits also guards writer
/// orchestration: a stale producer can never publish against a newer module state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorianPhase {
    #[default]
    Idle,
    Firing,
    AwaitingProducer,
    Validating,
    Publishing,
}

impl HistorianPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            HistorianPhase::Idle => "idle",
            HistorianPhase::Firing => "firing",
            HistorianPhase::AwaitingProducer => "awaiting_producer",
            HistorianPhase::Validating => "validating",
            HistorianPhase::Publishing => "publishing",
        }
    }
}

/// Inclusive ordinal range pinned for one historian run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorianChunkRange {
    pub from_ordinal: u64,
    pub to_ordinal: u64,
}

/// The durable historian state stored inside [`ModuleMeta`]. Idle keeps
/// `firing_seq` as the monotonic last-issued sequence and clears the in-flight
/// identifiers; abandon paths additionally set `failure_backoff_at_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorianDurableState {
    #[serde(default)]
    pub state: HistorianPhase,
    #[serde(default)]
    pub firing_seq: u64,
    #[serde(default)]
    pub chunk_range: Option<HistorianChunkRange>,
    #[serde(default)]
    pub chunk_fingerprint: String,
    #[serde(default)]
    pub producer_session_id: Option<String>,
    #[serde(default)]
    pub producer_run_id: Option<String>,
    #[serde(default)]
    pub fired_at_ms: Option<i64>,
    /// Session-level revert epoch observed when the chunk was assembled. It is copied
    /// into the firing state so a producer reattached after restart publishes against the
    /// same epoch it originally saw, not the session's current epoch.
    #[serde(default)]
    pub expected_revert_epoch: u64,
    #[serde(default)]
    pub failure_backoff_at_ms: Option<i64>,
    /// Human-readable detail of the most recent failed firing. The producer runs in a
    /// spawned task whose stderr a supervised deployment never captures, so the error
    /// must live in durable state to be diagnosable from a state dump. Cleared when a
    /// later firing establishes its producer run.
    #[serde(default)]
    pub last_failure: Option<String>,
    /// Why the most recent pass declined to fire (reason discriminant only, no numbers,
    /// so steady-state passes rewrite nothing). The twin of `last_failure` for the
    /// pre-fire half: a supervised rig cannot read the transform response's diagnostics
    /// block, so the skip branch must be readable from the state dump. Cleared on fire.
    #[serde(default)]
    pub last_no_fire: Option<String>,
}

impl Default for HistorianDurableState {
    fn default() -> Self {
        HistorianDurableState {
            state: HistorianPhase::Idle,
            firing_seq: 0,
            chunk_range: None,
            chunk_fingerprint: String::new(),
            producer_session_id: None,
            producer_run_id: None,
            fired_at_ms: None,
            expected_revert_epoch: 0,
            failure_backoff_at_ms: None,
            last_failure: None,
            last_no_fire: None,
        }
    }
}

/// Durable receive/complete/reject breadcrumbs for one session's transform passes.
/// Stored separately from `mc_cache_state` so a rejected pass can still leave a readable
/// trail without advancing the cache row_version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassTrace {
    pub last_received_at_ms: i64,
    pub last_completed_at_ms: i64,
    pub last_reject_error: Option<String>,
    pub last_reject_at_ms: Option<i64>,
    pub reject_count: u64,
    pub receive_count: u64,
}

/// A validated historian fact that may become a project memory. Validation owns
/// category semantics; the store performs only durable exact-content de-duplication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactCandidate {
    pub category: String,
    pub content: String,
    pub importance: Option<i32>,
    pub expires_at: Option<i64>,
    pub source_session_id: Option<String>,
}

/// A newly promoted project-memory row, returned so post-commit embedding can target
/// exactly the additive rows created by the publication transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedRef {
    pub memory_id: i64,
    pub content: String,
}

/// The stale-producer predicate checked inside the publish transaction before any
/// additive writes occur. Every field must match the durable state row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianPublishPredicate {
    pub firing_seq: u64,
    pub producer_run_id: String,
    pub chunk_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianPublishResult {
    pub row_version: u64,
    pub promoted_refs: Vec<PromotedRef>,
}

/// Session data read atomically for historian assembly. The epoch must be snapped
/// with the compartment set that determines the chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianAssemblySnapshot {
    pub compartments: Vec<StoredCompartment>,
    pub revert_epoch: u64,
}

/// Result of a deterministic revert re-cut. The caller must use the returned
/// row_version for the subsequent pass commit and patch the returned metadata fields
/// into the whole-blob ModuleMeta it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncateOutcome {
    pub revert_epoch: u64,
    pub last_recut: Option<String>,
    pub row_version: u64,
}

pub struct HistorianPublishRequest<'a> {
    pub session_id: &'a str,
    pub expected_row_version: Option<u64>,
    pub expected_revert_epoch: u64,
    pub predicate: &'a HistorianPublishPredicate,
    pub project_path: &'a str,
    pub compartments: &'a [StoredCompartment],
    pub facts: &'a [FactCandidate],
    pub publication_floor_ordinal: u64,
    pub chunk_transcript: Option<&'a str>,
}

/// Typed publish failures. CAS and state mismatches are deliberately separate so a
/// caller can tell "another writer already committed" from "this producer is stale."
#[derive(Debug)]
pub enum HistorianPublishError {
    Store(McStoreError),
    CasConflict {
        expected: Option<u64>,
        found: u64,
        reason: Option<String>,
    },
    StateMismatch {
        expected: Box<HistorianPublishPredicate>,
        found: Box<HistorianDurableState>,
    },
    InvalidState {
        state: String,
    },
    Serde(String),
}

impl std::fmt::Display for HistorianPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistorianPublishError::Store(e) => write!(f, "store: {e}"),
            HistorianPublishError::CasConflict {
                expected,
                found,
                reason,
            } => {
                if let Some(reason) = reason {
                    write!(
                        f,
                        "publish CAS conflict: expected {expected:?}, found {found}: {reason}"
                    )
                } else {
                    write!(f, "publish CAS conflict: expected {expected:?}, found {found}")
                }
            }
            HistorianPublishError::StateMismatch { expected, found } => write!(
                f,
                "historian publish state mismatch: expected seq {} run {} fingerprint {}, found {:?}",
                expected.firing_seq, expected.producer_run_id, expected.chunk_fingerprint, found
            ),
            HistorianPublishError::InvalidState { state } => {
                write!(f, "historian publish invalid state: {state}")
            }
            HistorianPublishError::Serde(e) => write!(f, "serde: {e}"),
        }
    }
}

impl std::error::Error for HistorianPublishError {}

impl From<McStoreError> for HistorianPublishError {
    fn from(e: McStoreError) -> Self {
        HistorianPublishError::Store(e)
    }
}

impl From<StoreError> for HistorianPublishError {
    fn from(e: StoreError) -> Self {
        HistorianPublishError::Store(McStoreError::Store(e))
    }
}

/// Persisted provider-usage ground truth used to keep pressure bands stable across
/// retries and restarts. A request-supplied non-zero value replaces this value; an
/// absent or all-zero request falls back to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleUsage {
    #[serde(default)]
    pub current_total_input_tokens: u64,
    #[serde(default)]
    pub context_limit_tokens: u64,
}

impl ModuleUsage {
    pub fn is_non_zero(&self) -> bool {
        self.current_total_input_tokens != 0 || self.context_limit_tokens != 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredExecuteState {
    pub reason: String,
}

/// Durable alarm state for a boundary-absent request that shares no prefix with the
/// session's held lineage. The transform arms this once and then serves matching
/// absent-shape traffic raw without more writes; only boundary-present recovery or a
/// later re-arm advances the diagnostic counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRewriteState {
    pub armed_at_ms: i64,
    pub absent_shape_fingerprint: String,
    #[serde(default)]
    pub absent_request_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_present_at_ms: Option<i64>,
}

/// The durable identity fingerprint for one block of a message. The transform records
/// the ordered vector per `mid` and rejects later drift instead of silently applying
/// frozen reductions to a different block list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockIdentity {
    pub kind_tag: String,
    pub byte_fingerprint: String,
}

/// Frozen CK-native synthetic todowrite pair persisted in module metadata.
///
/// The pair is replayed exactly at its stored anchor until the todo content changes.
/// Rebuilding or moving it would alter the exact prompt bytes seen by the provider,
/// so both CK messages are stored byte-complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrozenSyntheticTodoPair {
    /// Shared synthetic tool-call id used by both the assistant ToolCall and tool result.
    pub call_id: String,
    /// Real tail message id the pair is inserted after. None means no real tail existed
    /// when the pair was frozen, so it is appended at the output end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_mid: Option<String>,
    /// Frozen assistant-role CK message carrying the synthetic todowrite ToolCall.
    pub assistant_msg: CkWireMessage,
    /// Frozen tool-role CK message carrying the matching synthetic todowrite ToolResult.
    pub tool_msg: CkWireMessage,
}

#[derive(Debug, Clone, Deserialize)]
struct FrozenSyntheticTodoPairData {
    call_id: String,
    #[serde(default)]
    anchor_mid: Option<String>,
    assistant_msg: CkWireMessage,
    tool_msg: CkWireMessage,
}

impl<'de> Deserialize<'de> for FrozenSyntheticTodoPair {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let data = FrozenSyntheticTodoPairData::deserialize(deserializer)?;
        let mut assistant_msg = data.assistant_msg;
        let mut tool_msg = data.tool_msg;
        // Frozen synthetic todo messages are generated by this crate, not inbound
        // pass-through messages. Clear the retained Value after loading metadata so
        // replay uses the same canonical typed serialization as the original freeze.
        assistant_msg.mark_fully_typed();
        tool_msg.mark_fully_typed();
        Ok(Self {
            call_id: data.call_id,
            anchor_mid: data.anchor_mid,
            assistant_msg,
            tool_msg,
        })
    }
}

/// The non-CoreState durable blob: bootstrap + epoch-detection + coverage watermark.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModuleMeta {
    /// A baseline has been materialized at least once. Gates the bootstrap-Hard rule.
    pub initialized: bool,
    /// The render-config fingerprint as of the last Hard fold; an incoming pass whose
    /// fingerprint differs is an epoch change → Hard.
    pub last_render_config: String,
    /// The terminal covered ordinal as of the last baseline. Monotonic-absolute,
    /// never positional; can DECREASE on a revert-Hard.
    pub coverage_ordinal: Option<u64>,
    /// Last normalized `todowrite` view captured on a bust pass. This is deliberately
    /// session-scoped: a todo list is the working state of one conversation, not a
    /// project-shared memory or preference.
    #[serde(default)]
    pub last_todo_state: Option<String>,
    /// This session's `Today's date: ...` guidance line. Because it changes with the
    /// wall clock, we update it only during a pass that already rewrites cached content.
    #[serde(default)]
    pub guidance_date: String,
    /// Monotonic session-level epoch bumped atomically with a revert re-cut. Historian
    /// firings carry the epoch observed at assembly so stale publishers cannot append
    /// rows after the covered prefix has been truncated.
    #[serde(default)]
    pub revert_epoch: u64,
    /// Diagnostic for the most recent deterministic re-cut. It is stored with the epoch
    /// bump so state dumps explain which suffix was dropped without retaining history.
    #[serde(default)]
    pub last_recut: Option<String>,
    /// A boundary-absent, share-nothing request on a key that already has lineage. This
    /// is an alarmed raw-pass-through state, not a predicate for a future truncate.
    #[serde(default)]
    pub pending_rewrite: Option<PendingRewriteState>,
    /// Interleave edges between pending raw traffic and boundary-present traffic. The
    /// counter is diagnostic and drives the durable ambiguous alarm.
    #[serde(default)]
    pub pending_rewrite_trip_count: u32,
    /// True after repeated arm/clear interleaving proves two conversations are sharing
    /// one session key. Serving continues, but absent-shape traffic remains raw.
    #[serde(default)]
    pub pending_rewrite_ambiguous: bool,
    /// Durable loud detail for the pending/ambiguous rewrite alarm. It is separate from
    /// historian failures because no historian run owns this state.
    #[serde(default)]
    pub pending_rewrite_last_failure: Option<String>,
    /// Frozen CK-native synthetic todo pair plus the real tail message id it follows.
    /// Replays keep this exact position; only changed todo content moves it to a new
    /// tail end.
    #[serde(default)]
    pub synthetic_todo: Option<FrozenSyntheticTodoPair>,
    /// The content-digest revision the frozen m1 block was last rendered from. The
    /// classifier compares the incoming m1 content's revision against this to decide
    /// whether an m1 delta rides (Soft) WITHOUT rendering. 0 = placeholder (no delta).
    /// `serde(default)` so meta JSON persisted before this field loads cleanly.
    #[serde(default)]
    pub m1_revision: u64,

    // --- slice 4d-m0: the two-watermark coverage model + memory manifest ---
    // (all serde(default) so pre-4d meta JSON loads cleanly)
    /// The highest compartment `sequence` folded INTO m0. The "in-m0 vs riding-m1"
    /// divider — advances ONLY on a HARD fold. The m1 renderer treats compartments with
    /// `sequence > folded_compartment_seq` as new (renders them at P1); the HARD folds
    /// them and advances this. Distinct from `coverage_ordinal` (the m0+m1 coverage end /
    /// tail-trim point, which advances on a coverage-extending SOFT too).
    #[serde(default)]
    pub folded_compartment_seq: i64,
    /// The first ordinal covered by the compartment span reflected in `coverage_ordinal`.
    /// Leading system messages below this start are not summarized by compartments and
    /// must remain pass-through on full-array profiles.
    #[serde(default)]
    pub coverage_start_ordinal: Option<u64>,
    /// The highest compartment `sequence` reflected in `coverage_ordinal` after either a
    /// HARD fold or a coverage-extending SOFT. The transform compares the live scalar max
    /// against this before loading full rows for covered-system absorption, keeping steady
    /// defer passes off the compartment-row hot path. `None` means legacy metadata; callers
    /// fall back to `folded_compartment_seq`.
    #[serde(default)]
    pub coverage_compartment_seq: Option<i64>,
    /// The manifest of memory ids actually rendered into the frozen m0 (post-budget-trim).
    /// The supersede router uses membership here (NOT id<=max_memory_id, since a trim
    /// drops low-importance memories) to decide whether a memory UPDATE rides m1 as a
    /// `<memory-updates>` correction. Persisted atomically with the m0 bytes.
    #[serde(default)]
    pub rendered_memory_ids: Vec<i64>,
    /// The memory-mutation-log id folded as of the last HARD. m1 renders corrections with
    /// `id > memory_mutation_cursor`; a HARD reconciles them into m0 and advances this.
    #[serde(default)]
    pub memory_mutation_cursor: i64,
    /// The highest memory id folded into m0. m1 renders memories with `id > max_memory_id`
    /// as `<new-memories>`; a HARD folds them and advances this.
    #[serde(default)]
    pub max_memory_id: i64,
    /// The expiry cutoff FROZEN at the last HARD (the module clock at materialization). A
    /// memory's expiry is judged against THIS, not a live clock, so every later SOFT/defer
    /// compose sees the SAME memory set the m0 baseline was built against — a memory
    /// expiring between the HARD and a later pass must not change the rendered bytes.
    /// 0 before the first HARD.
    #[serde(default)]
    pub expiry_cutoff_ms: i64,

    // --- historian writer orchestration ---
    /// Durable single-flight state for the background historian. It is intentionally
    /// colocated with the cache meta blob: publish can CAS the state row and append
    /// rows in one SQLite transaction without introducing a second concurrency token.
    /// These fields never feed render bytes; they only decide whether a producer may
    /// publish or be reattached after restart.
    #[serde(default)]
    pub historian: HistorianDurableState,
    /// The trigger-only protected-tail floor advanced by a successful publication.
    /// This is distinct from `coverage_ordinal`: coverage drives render/splice output,
    /// while this floor only anchors future historian trigger selection.
    #[serde(default)]
    pub publication_floor_ordinal: Option<u64>,

    /// Ordered block identity vectors keyed by producer message id. Each vector stores
    /// the block kind and a fingerprint of the canonical reduction-accounting bytes, so
    /// a later request that changes a live message's block layout fails closed.
    #[serde(default)]
    pub block_identity_by_mid: BTreeMap<String, Vec<BlockIdentity>>,
    /// Record the newest non-synthetic flat block id seen in a successful live pass.
    /// When a note is created, this value is used as a best-effort pointer to the end
    /// of the conversation that the note refers to.
    #[serde(default)]
    pub newest_live_block_id: Option<String>,
    /// Last non-zero provider usage reported by the caller. Used when a retry or restart
    /// sends absent/zero usage, but overwritten by any later non-zero usage even when it
    /// decreases after reclaim.
    #[serde(default)]
    pub last_usage: Option<ModuleUsage>,
    /// The most recent serializer profile observed on this durable conversation key.
    #[serde(default)]
    pub last_serializer_profile: String,
    /// The request-local reduction surface state committed with the rendered identity.
    /// Missing legacy metadata is false, which preserves the dormant render path.
    #[serde(default)]
    pub cc_u1_active: bool,
    /// Reclaimable-token amount at the last Channel-1 append or suppression reset.
    #[serde(default)]
    pub channel1_last_nudge_undropped: i64,
    /// Last Channel-1 severity band that appended a reminder. Empty means no active band.
    #[serde(default)]
    pub channel1_last_nudge_level: String,
    /// Set by ctx_reduce after the agent has acted on a reminder. The next transform
    /// suppresses new Channel-1 appends while still replaying every stored append row.
    #[serde(default)]
    pub channel1_reduce_suppressed: bool,

    /// Highest tail ordinal observed on an execute pass that actually froze reductions.
    #[serde(default)]
    pub last_execute_ordinal: u64,
    /// Provider input-token sample from the prior emergency drop-producing pass.
    #[serde(default)]
    pub last_emergency_input_sample: f64,
    /// Whether the emergency idempotence sample is valid.
    #[serde(default)]
    pub has_prior_emergency_drop: bool,
    /// Execute intent recorded when mid-turn tool-use defers a scheduler execute.
    #[serde(default)]
    pub deferred_execute_state: Option<DeferredExecuteState>,
    /// Emergency drain latch active bit.
    #[serde(default)]
    pub emergency_drain_active: bool,
    /// Unix milliseconds when the drain latch was entered; 0 when inactive.
    #[serde(default)]
    pub emergency_drain_entered_at_ms: i64,
    /// Sparse response-recency anchor, piggybacked only on passes already committing.
    #[serde(default)]
    pub last_committed_pass_at_ms: i64,

    /// Tracks which shadow reset generation this record belongs to. Operations created
    /// before the most recent reset are rejected so they cannot write rows from an older
    /// session state.
    #[serde(default)]
    pub shadow_generation: u64,
    /// Monotonic sequence number for accepted shadow state-sync transactions. Zero is a
    /// valid first value, so callers must compare it directly instead of treating it as
    /// missing.
    #[serde(default)]
    pub shadow_seq: u64,
    /// Set when the shadow session first diverges from the source state. Quarantine is
    /// terminal for that generation: later passes increment the counter without adding
    /// duplicate divergence rows until a reset clears both fields.
    #[serde(default)]
    pub shadow_quarantined: bool,
    #[serde(default)]
    pub shadow_quarantined_pass_count: u64,
    /// Stores the last watermarks acknowledged from the sender, using the same
    /// compare-and-swap update as the mirror rows so restarts and retries see one
    /// consistent state.
    #[serde(default)]
    pub shadow_acked_watermarks: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentDrop {
    pub id: i64,
    pub target_id: String,
    pub queued_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendOutcome {
    pub queued: u64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagMintInput {
    pub block_id: String,
    pub kind: String,
    pub token_count: i64,
    pub source_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McTagRow {
    pub tag_number: i64,
    pub block_id: String,
    pub kind: String,
    pub token_count: i64,
    pub created_at_ms: i64,
    pub source_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel1AppendRow {
    pub block_id: String,
    pub reminder_text: String,
    pub fired_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserHintRow {
    pub block_id: String,
    pub hint_text: String,
    pub created_at: i64,
}

/// A stored compartment row (the m0/m1 history source). `sequence` is the
/// chronological order (1 = oldest).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredCompartment {
    pub sequence: i64,
    pub start_message: i64,
    pub end_message: i64,
    pub start_message_id: String,
    pub end_message_id: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub title: String,
    /// v2 P1 text, or the flat legacy body. Always present.
    pub content: String,
    /// v2 paraphrase tiers; None for legacy rows.
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    /// Decay rate (1..100), defaults to 50.
    pub importance: i32,
    pub episode_type: Option<String>,
    /// 1 = pre-v2 flat compartment, 0 = v2 tiered.
    pub legacy: i32,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryRevision {
    pub project_paths: Vec<String>,
    pub max_memory_id: i64,
    pub mutation_cursor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRenderSnapshot {
    pub memories: Vec<StoredMemory>,
    pub revision: MemoryRevision,
}

/// A project memory row projected for rendering into the prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemory {
    pub id: i64,
    pub project_path: String,
    pub category: String,
    pub content: String,
    /// Decay-rate / budget-trim ordering signal (1..100); None when unclassified.
    pub importance: Option<i32>,
    /// "active" | "permanent" | "archived" — the render set is active+permanent.
    pub status: String,
    pub expires_at: Option<i64>,
    /// Set when a later memory has replaced this one; consulted when rendering the list
    /// of memory corrections (a superseded memory renders as "X → Y").
    pub superseded_by_memory_id: Option<i64>,
    pub updated_at: i64,
}

/// A complete `mc_memories` row for tool-side guards and lossless mutations. The render
/// path intentionally reads a smaller projection; mutation ports use this shape so they
/// can preserve status, ownership, merge metadata, and cache-invalidation columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemoryFull {
    pub id: i64,
    pub project_path: String,
    pub category: String,
    pub content: String,
    pub normalized_hash: String,
    pub importance: Option<i32>,
    pub scope: String,
    pub shareable: i32,
    pub source_session_id: Option<String>,
    pub source_type: Option<String>,
    pub seen_count: i64,
    pub retrieval_count: i64,
    pub first_seen_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
    pub last_retrieved_at: Option<i64>,
    pub status: String,
    pub expires_at: Option<i64>,
    pub verification_status: String,
    pub verified_at: Option<i64>,
    pub classified_at: Option<i64>,
    pub superseded_by_memory_id: Option<i64>,
    pub merged_from: Option<String>,
    pub metadata_json: Option<String>,
}

/// Inputs for an additive ctx_memory write. Duplicate detection follows the plugin's
/// normalized-content hash (`lowercase → collapse whitespace → MD5`): a matching
/// `(project_path, category, normalized_hash)` returns the existing row id instead of
/// inserting a new row.
#[derive(Debug, Clone, Copy)]
pub struct InsertMemoryInput<'a> {
    pub project_path: &'a str,
    pub category: &'a str,
    pub content: &'a str,
    pub source_session_id: Option<&'a str>,
    pub source_type: Option<&'a str>,
    pub importance: Option<i32>,
    pub expires_at: Option<i64>,
    pub metadata_json: Option<&'a str>,
    pub now_ms: i64,
}

/// Minimal memory search row. The module ranks/snippets the results; the store owns the
/// SQL LIKE and workspace-visibility read so search shares the render path's boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemorySearchRow {
    pub id: i64,
    pub project_path: String,
    pub category: String,
    pub content: String,
    pub updated_at: i64,
}

/// Minimal compartment search row. `sequence` is the durable row id inside a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredCompartmentSearchRow {
    pub sequence: i64,
    pub title: String,
    pub content: String,
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChunkTranscript {
    pub compartment_seq: i64,
    pub start_ordinal: i64,
    pub end_ordinal: i64,
    pub transcript: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct NoteInput<'a> {
    pub project_path: &'a str,
    pub session_id: &'a str,
    pub content: &'a str,
    pub surface_condition: Option<&'a str>,
    pub anchor_block_id: Option<&'a str>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNote {
    pub id: i64,
    pub project_path: String,
    pub session_id: String,
    pub content: String,
    pub status: String,
    pub surface_condition: Option<String>,
    pub anchor_block_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNoteSearchRow {
    pub id: i64,
    pub content: String,
    pub status: String,
    pub surface_condition: Option<String>,
    pub updated_at_ms: i64,
}

/// A loaded per-session row: the core state, the meta blob, and the CAS token.
#[derive(Debug, Clone)]
pub struct LoadedState {
    pub core: CoreState,
    pub meta: ModuleMeta,
    /// The row_version read from disk; pass it back to [`McStore::commit`] as the CAS
    /// expectation. `None` when no row existed yet (first bootstrap → INSERT path).
    pub row_version: Option<u64>,
}

/// Represents one mirrored project-memory row from shadow state-sync. It keeps the
/// original memory id unchanged because that id is written into prompt data and
/// referenced by mutation rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowMemoryRow {
    pub id: i64,
    pub project_path: String,
    pub category: String,
    pub content: String,
    pub normalized_hash: String,
    pub importance: Option<i32>,
    pub scope: String,
    pub shareable: i32,
    pub source_session_id: Option<String>,
    pub source_type: Option<String>,
    pub seen_count: i64,
    pub retrieval_count: i64,
    pub first_seen_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
    pub last_retrieved_at: Option<i64>,
    pub status: String,
    pub expires_at: Option<i64>,
    pub verification_status: String,
    pub verified_at: Option<i64>,
    pub classified_at: Option<i64>,
    pub superseded_by_memory_id: Option<i64>,
    pub merged_from: Option<String>,
    pub metadata_json: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowWorkspaceMemberRow {
    pub project_path: String,
    pub display_name: String,
    pub display_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowWorkspaceRow {
    pub name: String,
    pub share_categories: Vec<String>,
    pub members: Vec<ShadowWorkspaceMemberRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowMemoryMutationRow {
    pub project_path: String,
    pub mutation: StoredMemoryMutation,
}

pub struct ShadowStateSyncRequest<'a> {
    pub session_id: &'a str,
    pub shadow_project_path: &'a str,
    pub shadow_generation: u64,
    pub expected_shadow_seq: u64,
    /// The producer's current flat compaction boundary. Present only on a full seed.
    pub seed_boundary_id: Option<&'a str>,
    pub compartments: &'a [StoredCompartment],
    pub memories: &'a [ShadowMemoryRow],
    pub memory_mutations: &'a [ShadowMemoryMutationRow],
    pub workspace: Option<&'a ShadowWorkspaceRow>,
    pub last_todo_state: Option<String>,
    pub acked_watermarks: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowStateSyncResult {
    pub shadow_generation: u64,
    pub shadow_seq: u64,
    pub row_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateImportResult {
    pub imported: usize,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateImportPreflight {
    Ready,
    Duplicate { imported: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateImportValidationError {
    SeqNotIncreasing { previous: i64, current: i64 },
    RangeInvalid { sequence: i64 },
    RangesOverlap { previous: i64, current: i64 },
    P1Empty { sequence: i64 },
    EndMessageIdInvalid { sequence: i64 },
}

impl StateImportValidationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SeqNotIncreasing { .. } => "seq_not_increasing",
            Self::RangeInvalid { .. } => "range_invalid",
            Self::RangesOverlap { .. } => "ranges_overlap",
            Self::P1Empty { .. } => "p1_empty",
            Self::EndMessageIdInvalid { .. } => "end_message_id_invalid",
        }
    }
}

#[derive(Debug)]
pub enum StateImportError {
    Store(McStoreError),
    SessionNotEmpty,
    Validation(StateImportValidationError),
}

#[derive(Debug)]
pub enum ShadowStateSyncError {
    Store(McStoreError),
    GenerationMismatch { expected: u64, found: u64 },
    SeqMismatch { expected: u64, found: u64 },
    InvalidSeedBoundary { declared: String, detail: String },
    Serde(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowResetResult {
    pub shadow_generation: u64,
    pub shadow_seq: u64,
    pub row_version: u64,
}

pub struct ShadowDivergenceRecord<'a> {
    pub session_id: &'a str,
    pub shadow_generation: u64,
    pub pass_seq: u64,
    pub class: &'a str,
    pub first_mid: Option<&'a str>,
    pub first_block: Option<&'a str>,
    pub first_field: Option<&'a str>,
    pub ts_prefix: &'a str,
    pub rs_prefix: &'a str,
    pub first_diff_offset: Option<u64>,
    pub ts_window: &'a str,
    pub rs_window: &'a str,
    pub normalizations_json: &'a str,
    pub ts_decision_json: &'a str,
    pub rs_decision_json: &'a str,
    pub state_hash: &'a str,
    pub created_at_ms: i64,
    pub quarantine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDivergenceWriteResult {
    pub quarantined: bool,
    pub row_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDivergenceRow {
    pub id: i64,
    pub session_id: String,
    pub pass_seq: u64,
    pub class: String,
    pub first_mid: Option<String>,
    pub first_block: Option<String>,
    pub first_field: Option<String>,
    pub ts_prefix: String,
    pub rs_prefix: String,
    pub first_diff_offset: Option<u64>,
    pub ts_window: String,
    pub rs_window: String,
    pub normalizations_json: String,
    pub ts_decision_json: String,
    pub rs_decision_json: String,
    pub state_hash: String,
    pub created_at_ms: i64,
}

#[derive(Debug)]
pub enum McStoreError {
    Store(StoreError),
    /// The on-disk row_version moved under us (a concurrent writer committed first).
    /// The caller re-loads and re-steps.
    CasConflict {
        expected: Option<u64>,
        found: u64,
    },
    Serde(String),
    MemoryDuplicateContent {
        id: i64,
    },
}

impl std::fmt::Display for McStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McStoreError::Store(e) => write!(f, "store: {e}"),
            McStoreError::CasConflict { expected, found } => {
                write!(f, "cas conflict: expected {expected:?}, found {found}")
            }
            McStoreError::Serde(e) => write!(f, "serde: {e}"),
            McStoreError::MemoryDuplicateContent { id } => {
                write!(f, "memory content already exists as ID {id}")
            }
        }
    }
}
impl std::error::Error for McStoreError {}
impl From<StoreError> for McStoreError {
    fn from(e: StoreError) -> Self {
        McStoreError::Store(e)
    }
}

impl std::fmt::Display for StateImportValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SeqNotIncreasing { previous, current } => write!(
                f,
                "compartment seq must be strictly increasing: {current} followed {previous}"
            ),
            Self::RangeInvalid { sequence } => {
                write!(
                    f,
                    "compartment {sequence} has start_message after end_message"
                )
            }
            Self::RangesOverlap { previous, current } => write!(
                f,
                "compartment {current} overlaps or precedes compartment {previous}"
            ),
            Self::P1Empty { sequence } => {
                write!(f, "compartment {sequence} has an empty p1")
            }
            Self::EndMessageIdInvalid { sequence } => write!(
                f,
                "compartment {sequence} end_message_id is not a parseable mid#idx"
            ),
        }
    }
}

impl std::error::Error for StateImportValidationError {}

impl std::fmt::Display for StateImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "store: {error}"),
            Self::SessionNotEmpty => write!(f, "session already has durable state"),
            Self::Validation(error) => write!(f, "{}: {error}", error.code()),
        }
    }
}

impl std::error::Error for StateImportError {}

impl From<McStoreError> for StateImportError {
    fn from(error: McStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<StoreError> for StateImportError {
    fn from(error: StoreError) -> Self {
        Self::Store(McStoreError::Store(error))
    }
}

impl std::fmt::Display for ShadowStateSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShadowStateSyncError::Store(e) => write!(f, "store: {e}"),
            ShadowStateSyncError::GenerationMismatch { expected, found } => write!(
                f,
                "shadow generation mismatch: expected {expected}, found {found}"
            ),
            ShadowStateSyncError::SeqMismatch { expected, found } => {
                write!(f, "shadow seq mismatch: expected {expected}, found {found}")
            }
            ShadowStateSyncError::InvalidSeedBoundary { declared, detail } => {
                write!(f, "invalid seed boundary {declared:?}: {detail}")
            }
            ShadowStateSyncError::Serde(e) => write!(f, "serde: {e}"),
        }
    }
}

impl std::error::Error for ShadowStateSyncError {}

impl From<McStoreError> for ShadowStateSyncError {
    fn from(e: McStoreError) -> Self {
        ShadowStateSyncError::Store(e)
    }
}

impl From<StoreError> for ShadowStateSyncError {
    fn from(e: StoreError) -> Self {
        ShadowStateSyncError::Store(McStoreError::Store(e))
    }
}

/// Outcome of the fenced commit txn: either the new row_version, or a CAS conflict
/// carrying the version observed on disk. Modeled as a return value (not an error)
/// so a conflicting pass commits an empty txn and the caller re-loads cleanly.
enum MemoryMutationOutcome {
    NotFound,
    Applied(Box<Option<StoredMemoryFull>>),
    Duplicate(i64),
}

enum CommitOutcome {
    Committed(u64),
    CasConflict(u64),
}

enum PublishTxnOutcome {
    Committed(HistorianPublishResult),
    CasConflict { found: u64, reason: Option<String> },
    StateMismatch(HistorianDurableState),
    InvalidState(String),
    Serde(String),
}

enum TruncateTxnOutcome {
    Committed(TruncateOutcome),
    CasConflict(u64),
    Serde(String),
}

enum StateImportTxnOutcome {
    Imported(usize),
    Duplicate(usize),
    SessionNotEmpty,
    Validation(StateImportValidationError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedSeedBoundary {
    boundary_id: String,
    coverage_start_ordinal: u64,
    coverage_end_ordinal: u64,
    max_sequence: i64,
}

fn split_flat_block_id(id: &str) -> Option<(&str, u64)> {
    let (mid, index) = id.rsplit_once('#')?;
    if mid.is_empty() || mid.contains('#') {
        return None;
    }
    Some((mid, index.parse().ok()?))
}

pub fn validate_state_import_compartments(
    compartments: &[StoredCompartment],
) -> Result<(), StateImportValidationError> {
    let mut previous: Option<&StoredCompartment> = None;
    for compartment in compartments {
        if compartment.start_message > compartment.end_message {
            return Err(StateImportValidationError::RangeInvalid {
                sequence: compartment.sequence,
            });
        }
        if compartment
            .p1
            .as_deref()
            .is_none_or(|p1| p1.trim().is_empty())
        {
            return Err(StateImportValidationError::P1Empty {
                sequence: compartment.sequence,
            });
        }
        if split_flat_block_id(&compartment.end_message_id).is_none() {
            return Err(StateImportValidationError::EndMessageIdInvalid {
                sequence: compartment.sequence,
            });
        }
        if let Some(previous) = previous {
            if compartment.sequence <= previous.sequence {
                return Err(StateImportValidationError::SeqNotIncreasing {
                    previous: previous.sequence,
                    current: compartment.sequence,
                });
            }
            if compartment.start_message <= previous.end_message {
                return Err(StateImportValidationError::RangesOverlap {
                    previous: previous.sequence,
                    current: compartment.sequence,
                });
            }
        }
        previous = Some(compartment);
    }
    Ok(())
}

fn session_has_durable_state(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> rusqlite::Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM mc_cache_state WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_compartments WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_tags WHERE session_id = ?1
             UNION ALL SELECT 1 FROM pending_agent_drops WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_reduce_command_ledger WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_channel1_appends WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_user_hints WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_pass_trace WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_chunk_transcripts WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_notes WHERE session_id = ?1
         )",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn validated_seed_boundary(
    declared: &str,
    compartments: &[StoredCompartment],
) -> Result<ValidatedSeedBoundary, String> {
    let (declared_mid, declared_index) = split_flat_block_id(declared)
        .ok_or_else(|| "declared identity must be a parseable <mid>#<index> flat id".to_string())?;
    let mut ordered = compartments.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|compartment| compartment.sequence);
    let tail = ordered
        .last()
        .copied()
        .ok_or_else(|| "a boundary cannot be adopted without seeded compartments".to_string())?;

    if ordered.iter().any(|compartment| {
        compartment.start_message < 0 || compartment.end_message < compartment.start_message
    }) {
        return Err(
            "seeded compartment ordinal ranges must be non-negative and ordered".to_string(),
        );
    }
    for pair in ordered.windows(2) {
        if pair[0].sequence == pair[1].sequence {
            return Err("seeded compartment sequences must be unique".to_string());
        }
        if pair[1].start_message <= pair[0].end_message {
            return Err(format!(
                "seeded compartment ranges overlap at ordinals {} and {}",
                pair[0].end_message, pair[1].start_message
            ));
        }
    }

    let (tail_mid, tail_index) = split_flat_block_id(&tail.end_message_id).ok_or_else(|| {
        "the highest-sequence compartment must carry a parseable flat end_message_id".to_string()
    })?;
    if declared_mid != tail_mid {
        return Err(format!(
            "declared message {declared_mid:?} did not match tail compartment message {tail_mid:?}"
        ));
    }
    if declared_index != tail_index {
        return Err(format!(
            "declared block index {declared_index} did not match tail compartment end-block index {tail_index}"
        ));
    }

    Ok(ValidatedSeedBoundary {
        // The compartment publisher's end-block form is canonical even if a future
        // sender derives the same identity through a different marker representation.
        boundary_id: tail.end_message_id.clone(),
        coverage_start_ordinal: ordered[0].start_message as u64,
        coverage_end_ordinal: tail.end_message as u64,
        max_sequence: tail.sequence,
    })
}

enum ShadowSyncTxnOutcome {
    Committed(ShadowStateSyncResult),
    GenerationMismatch { found: u64 },
    SeqMismatch { found: u64 },
    InvalidSeedBoundary { declared: String, detail: String },
    Serde(String),
}

enum ShadowDivergenceTxnOutcome {
    Committed(ShadowDivergenceWriteResult),
    GenerationMismatch { found: u64 },
    Serde(String),
}

/// The Magic Context cache-state store: one single-writer SQLite handle for the
/// module's lifetime.
pub struct McStore {
    inner: SqliteStore,
}

impl McStore {
    /// Open from a resolved descriptor (acquires the single-writer lease) and apply
    /// the cache-state migration chain. Open exactly ONCE per module lifetime.
    pub fn open(descriptor: &StorageDescriptor) -> Result<Self, McStoreError> {
        let inner = open_sqlite(descriptor)?;
        inner.migrate(NS, MIGRATIONS)?;
        Ok(McStore { inner })
    }

    /// Load a session's persisted state. Returns defaults (uninitialized, no row)
    /// when the session has never been seen — the classifier then bootstraps.
    pub fn load(&self, session_id: &str) -> Result<LoadedState, McStoreError> {
        let row = self.inner.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? as u64,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .ok())
        })?;

        match row {
            None => Ok(LoadedState {
                core: CoreState::default(),
                meta: ModuleMeta::default(),
                row_version: None,
            }),
            Some((rv, core_json, meta_json)) => Ok(LoadedState {
                core: serde_json::from_str(&core_json)
                    .map_err(|e| McStoreError::Serde(e.to_string()))?,
                meta: serde_json::from_str(&meta_json)
                    .map_err(|e| McStoreError::Serde(e.to_string()))?,
                row_version: Some(rv),
            }),
        }
    }

    /// Record that the module accepted a transform request for this session. This is a
    /// plain one-statement UPSERT outside the fenced cache-state transaction so the
    /// observability write never contends with or extends the pass commit.
    pub fn trace_pass_received(&self, session_id: &str, now_ms: i64) -> Result<(), McStoreError> {
        self.inner.with_conn(|conn| {
            conn.execute(
                "INSERT INTO mc_pass_trace (
                     session_id,
                     last_received_at_ms,
                     last_completed_at_ms,
                     last_reject_error,
                     last_reject_at_ms,
                     reject_count,
                     receive_count
                 ) VALUES (?1, ?2, 0, NULL, NULL, 0, 1)
                 ON CONFLICT(session_id) DO UPDATE SET
                     last_received_at_ms = excluded.last_received_at_ms,
                     receive_count = mc_pass_trace.receive_count + 1",
                params![session_id, now_ms],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Record that a transform request finished successfully. This remains outside the
    /// fenced cache-state transaction so a pass completion breadcrumb cannot alter CAS
    /// semantics or hold the commit transaction open longer than the cache write itself.
    pub fn trace_pass_completed(&self, session_id: &str, now_ms: i64) -> Result<(), McStoreError> {
        self.inner.with_conn(|conn| {
            conn.execute(
                "INSERT INTO mc_pass_trace (
                     session_id,
                     last_received_at_ms,
                     last_completed_at_ms,
                     last_reject_error,
                     last_reject_at_ms,
                     reject_count,
                     receive_count
                 ) VALUES (?1, 0, ?2, NULL, NULL, 0, 0)
                 ON CONFLICT(session_id) DO UPDATE SET
                     last_completed_at_ms = excluded.last_completed_at_ms",
                params![session_id, now_ms],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Record the last rejected transform for a session. The error string is capped so
    /// the durable diagnostic stays readable even if an upstream failure produces a huge
    /// message. Like the other trace writes, this is a single plain UPSERT outside the
    /// fenced cache-state transaction.
    pub fn trace_pass_rejected(
        &self,
        session_id: &str,
        error: &str,
        now_ms: i64,
    ) -> Result<(), McStoreError> {
        let error = capped_trace_error(error);
        self.inner.with_conn(|conn| {
            conn.execute(
                "INSERT INTO mc_pass_trace (
                     session_id,
                     last_received_at_ms,
                     last_completed_at_ms,
                     last_reject_error,
                     last_reject_at_ms,
                     reject_count,
                     receive_count
                 ) VALUES (?1, 0, 0, ?2, ?3, 1, 0)
                 ON CONFLICT(session_id) DO UPDATE SET
                     last_reject_error = excluded.last_reject_error,
                     last_reject_at_ms = excluded.last_reject_at_ms,
                     reject_count = mc_pass_trace.reject_count + 1",
                params![session_id, error, now_ms],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Load the durable pass breadcrumbs for one session, if any have been written.
    pub fn load_pass_trace(&self, session_id: &str) -> Result<Option<PassTrace>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT
                     last_received_at_ms,
                     last_completed_at_ms,
                     last_reject_error,
                     last_reject_at_ms,
                     reject_count,
                     receive_count
                 FROM mc_pass_trace
                 WHERE session_id = ?1",
                params![session_id],
                |r| {
                    Ok(PassTrace {
                        last_received_at_ms: r.get(0)?,
                        last_completed_at_ms: r.get(1)?,
                        last_reject_error: r.get(2)?,
                        last_reject_at_ms: r.get(3)?,
                        reject_count: r.get::<_, i64>(4)? as u64,
                        receive_count: r.get::<_, i64>(5)? as u64,
                    })
                },
            )
            .optional()
        })?)
    }

    /// Append flat block ids requested by ctx_reduce to the durable per-session queue.
    /// Duplicate pending ids are ignored so repeated command delivery is harmless.
    pub fn append_pending_agent_drops(
        &self,
        session_id: &str,
        target_ids: &[String],
        queued_at_ms: i64,
    ) -> Result<usize, McStoreError> {
        let outcome = self.append_pending_agent_drops_with_command(
            session_id,
            None,
            target_ids,
            queued_at_ms,
        )?;
        Ok(outcome.queued as usize)
    }

    /// Append ctx_reduce drops and, when supplied, durably record the command that requested
    /// them. A repeated command is acknowledged without touching pending queue rows.
    pub fn append_pending_agent_drops_with_command(
        &self,
        session_id: &str,
        command_id: Option<&str>,
        target_ids: &[String],
        queued_at_ms: i64,
    ) -> Result<AppendOutcome, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            if let Some(command_id) = command_id {
                let recorded = tx.execute(
                    "INSERT OR IGNORE INTO mc_reduce_command_ledger
                         (session_id, command_id, queued_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![session_id, command_id, queued_at_ms],
                )?;
                if recorded == 0 {
                    return Ok(AppendOutcome {
                        queued: 0,
                        duplicate: true,
                    });
                }
            }

            let mut queued = 0u64;
            for target_id in target_ids {
                let target_id = target_id.trim();
                if target_id.is_empty() {
                    continue;
                }
                queued += tx.execute(
                    "INSERT OR IGNORE INTO pending_agent_drops (session_id, target_id, queued_at)
                     VALUES (?1, ?2, ?3)",
                    params![session_id, target_id, queued_at_ms],
                )? as u64;
            }

            // Command ids are lineage-durable. Pruning would make an old outcome-unknown
            // retry destructive again, so rows leave only with real lineage teardown.

            Ok(AppendOutcome {
                queued,
                duplicate: false,
            })
        })?;
        Ok(outcome)
    }

    /// Load queued ctx_reduce drops in the deterministic drain order.
    pub fn load_pending_agent_drops(
        &self,
        session_id: &str,
    ) -> Result<Vec<PendingAgentDrop>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, target_id, queued_at
                 FROM pending_agent_drops
                 WHERE session_id = ?1
                 ORDER BY queued_at ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |r| {
                Ok(PendingAgentDrop {
                    id: r.get(0)?,
                    target_id: r.get(1)?,
                    queued_at_ms: r.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?)
    }

    /// Mint tag rows for newly-observed block ids and return every requested row.
    /// Existing rows keep their original numbers; fresh rows consume the next numbers
    /// in the caller's order inside one transaction.
    pub fn mint_or_get_tags(
        &self,
        session_id: &str,
        inputs: &[TagMintInput],
        created_at_ms: i64,
    ) -> Result<Vec<McTagRow>, McStoreError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            let mut out = Vec::with_capacity(inputs.len());
            for input in inputs {
                let block_id = input.block_id.trim();
                if block_id.is_empty() {
                    continue;
                }
                if let Some(row) = tx
                    .query_row(
                        "SELECT tag_number, block_id, kind, token_count, created_at_ms, source_bytes
                         FROM mc_tags
                         WHERE session_id = ?1 AND block_id = ?2",
                        params![session_id, block_id],
                        tag_row_from_sql,
                    )
                    .optional()?
                {
                    out.push(row);
                    continue;
                }
                let next = tx.query_row(
                    "SELECT COALESCE(MAX(tag_number), 0) + 1 FROM mc_tags WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get::<_, i64>(0),
                )?;
                tx.execute(
                    "INSERT INTO mc_tags
                         (session_id, tag_number, block_id, kind, token_count, created_at_ms, source_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        session_id,
                        next,
                        block_id,
                        input.kind.as_str(),
                        input.token_count.max(0),
                        created_at_ms,
                        input.source_bytes.as_slice(),
                    ],
                )?;
                out.push(McTagRow {
                    tag_number: next,
                    block_id: block_id.to_string(),
                    kind: input.kind.clone(),
                    token_count: input.token_count.max(0),
                    created_at_ms,
                    source_bytes: input.source_bytes.clone(),
                });
            }
            Ok(out)
        })?)
    }

    /// Load all minted tags for a session in tag-number order.
    pub fn load_tags_for_session(&self, session_id: &str) -> Result<Vec<McTagRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT tag_number, block_id, kind, token_count, created_at_ms, source_bytes
                 FROM mc_tags
                 WHERE session_id = ?1
                 ORDER BY tag_number ASC",
            )?;
            let rows = stmt.query_map(params![session_id], tag_row_from_sql)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?)
    }

    /// Sum stored token counts for a caller-selected block-id set.
    pub fn sum_tag_token_counts_for_blocks(
        &self,
        session_id: &str,
        block_ids: &HashSet<String>,
    ) -> Result<i64, McStoreError> {
        if block_ids.is_empty() {
            return Ok(0);
        }
        let rows = self.load_tags_for_session(session_id)?;
        Ok(rows
            .into_iter()
            .filter(|row| block_ids.contains(&row.block_id))
            .map(|row| row.token_count.max(0))
            .sum())
    }

    /// Insert one Channel-1 append row if this block has not already received one.
    pub fn append_channel1_nudge(
        &self,
        session_id: &str,
        block_id: &str,
        reminder_text: &str,
        fired_at_ms: i64,
    ) -> Result<bool, McStoreError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO mc_channel1_appends
                     (session_id, block_id, reminder_text, fired_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, block_id, reminder_text, fired_at_ms],
            )?;
            Ok(inserted > 0)
        })?)
    }

    /// Load stored Channel-1 append bytes in deterministic order.
    pub fn load_channel1_appends(
        &self,
        session_id: &str,
    ) -> Result<Vec<Channel1AppendRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT block_id, reminder_text, fired_at_ms
                 FROM mc_channel1_appends
                 WHERE session_id = ?1
                 ORDER BY fired_at_ms ASC, block_id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |r| {
                Ok(Channel1AppendRow {
                    block_id: r.get(0)?,
                    reminder_text: r.get(1)?,
                    fired_at_ms: r.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?)
    }

    /// Return whether a block may receive the next first-sight hint decision.
    /// Tag numbers preserve first-observation order, so any decided higher number closes
    /// the frontier even when that later block is absent from the current request.
    pub fn user_hint_frontier_open(
        &self,
        session_id: &str,
        block_id: &str,
        tag_number: i64,
    ) -> Result<bool, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT
                     NOT EXISTS(
                         SELECT 1 FROM mc_user_hints
                         WHERE session_id = ?1 AND block_id = ?2
                     )
                     AND NOT EXISTS(
                         SELECT 1
                         FROM mc_user_hints AS hint
                         JOIN mc_tags AS tag
                           ON tag.session_id = hint.session_id
                          AND tag.block_id = hint.block_id
                         WHERE hint.session_id = ?1 AND tag.tag_number > ?3
                     )",
                params![session_id, block_id, tag_number],
                |row| row.get(0),
            )
        })?)
    }

    /// Persist one auto-search decision, including an empty no-result decision.
    pub fn append_user_hint(
        &self,
        session_id: &str,
        block_id: &str,
        hint_text: &str,
        created_at: i64,
    ) -> Result<bool, McStoreError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO mc_user_hints
                     (session_id, block_id, hint_text, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, block_id, hint_text, created_at],
            )?;
            Ok(inserted > 0)
        })?)
    }

    /// Load exact auto-search overlay bytes and durable empty decisions.
    pub fn load_user_hints(&self, session_id: &str) -> Result<Vec<UserHintRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT block_id, hint_text, created_at
                 FROM mc_user_hints
                 WHERE session_id = ?1
                 ORDER BY created_at ASC, block_id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(UserHintRow {
                    block_id: row.get(0)?,
                    hint_text: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?)
    }

    /// Check whether an import attempt may begin without staging anything durably.
    /// The final commit repeats these predicates inside its fenced transaction.
    pub fn preflight_state_import(
        &self,
        session_id: &str,
        import_id: &str,
    ) -> Result<StateImportPreflight, StateImportError> {
        let status = self.inner.with_conn(|conn| {
            let completed = conn
                .query_row(
                    "SELECT import_id, imported_count FROM mc_state_imports WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let nonempty = session_has_durable_state(conn, session_id)?;
            Ok((completed, nonempty))
        })?;
        match status {
            (Some((completed_id, imported)), _) if completed_id == import_id => {
                Ok(StateImportPreflight::Duplicate {
                    imported: imported.max(0) as usize,
                })
            }
            (Some(_), _) | (None, true) => Err(StateImportError::SessionNotEmpty),
            (None, false) => Ok(StateImportPreflight::Ready),
        }
    }

    /// Atomically seed compartments into a never-used real-session key and record the
    /// completed import id. No cache row is created: the normal first transform observes
    /// the compartments with an empty boundary and performs the bootstrap HARD fold that
    /// mints the live boundary anchor.
    pub fn commit_state_import(
        &self,
        session_id: &str,
        import_id: &str,
        compartments: &[StoredCompartment],
        completed_at_ms: i64,
    ) -> Result<StateImportResult, StateImportError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let completed = tx
                .query_row(
                    "SELECT import_id, imported_count FROM mc_state_imports WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            if let Some((completed_id, imported)) = completed {
                if completed_id == import_id {
                    return Ok(StateImportTxnOutcome::Duplicate(imported.max(0) as usize));
                }
                return Ok(StateImportTxnOutcome::SessionNotEmpty);
            }

            // This is the fresh-row form of the cache-state CAS. The predicate and all
            // compartment writes share one fenced transaction, so a racing bootstrap
            // cannot slip state between the emptiness check and the imported rows.
            if session_has_durable_state(tx, session_id)? {
                return Ok(StateImportTxnOutcome::SessionNotEmpty);
            }
            if let Err(error) = validate_state_import_compartments(compartments) {
                return Ok(StateImportTxnOutcome::Validation(error));
            }

            for compartment in compartments {
                insert_compartment_tx(tx, session_id, compartment.sequence, compartment)?;
            }
            tx.execute(
                "INSERT INTO mc_state_imports
                     (session_id, import_id, imported_count, completed_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session_id,
                    import_id,
                    compartments.len() as i64,
                    completed_at_ms
                ],
            )?;
            Ok(StateImportTxnOutcome::Imported(compartments.len()))
        })?;

        match outcome {
            StateImportTxnOutcome::Imported(imported) => Ok(StateImportResult {
                imported,
                duplicate: false,
            }),
            StateImportTxnOutcome::Duplicate(imported) => Ok(StateImportResult {
                imported,
                duplicate: true,
            }),
            StateImportTxnOutcome::SessionNotEmpty => Err(StateImportError::SessionNotEmpty),
            StateImportTxnOutcome::Validation(error) => Err(StateImportError::Validation(error)),
        }
    }

    /// Commit new state under the row_version CAS, inside the epoch-fenced txn.
    ///
    /// `expected` is the row_version from [`load`] (`None` = expect no row → INSERT).
    /// On success the row_version is bumped by one. A `CasConflict` means a
    /// concurrent writer won; the caller re-loads and re-steps. Call ONLY when
    /// durable state changed — a pure SoftPlus replay must skip the commit entirely
    /// so the no-write-on-defer guarantee holds.
    pub fn commit(
        &self,
        session_id: &str,
        expected: Option<u64>,
        core: &CoreState,
        meta: &ModuleMeta,
    ) -> Result<u64, McStoreError> {
        self.commit_with_consumed_drops(session_id, expected, core, meta, &[], None)
    }

    /// Commit cache state and delete consumed ctx_reduce queue rows in one fenced tx.
    pub fn commit_with_consumed_drops(
        &self,
        session_id: &str,
        expected: Option<u64>,
        core: &CoreState,
        meta: &ModuleMeta,
        consumed_drop_ids: &[i64],
        memory_revision: Option<&MemoryRevision>,
    ) -> Result<u64, McStoreError> {
        let core_json =
            serde_json::to_string(core).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let meta_json =
            serde_json::to_string(meta).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let next = expected.unwrap_or(0) + 1;

        let outcome = self.inner.with_conn_fenced(|tx| {
            // Read the current row_version inside the fenced txn; NO_ROW when absent.
            let current: i64 = tx.query_row(
                "SELECT COALESCE((SELECT row_version FROM mc_cache_state WHERE session_id = ?1), ?2)",
                params![session_id, NO_ROW],
                |r| r.get(0),
            )?;

            let cas_ok = match expected {
                None => current == NO_ROW,
                Some(v) => current == v as i64,
            };
            if !cas_ok {
                // Empty txn (commits nothing); the caller re-loads and re-steps.
                return Ok(CommitOutcome::CasConflict(current.max(0) as u64));
            }
            if let Some(revision) = memory_revision.filter(|value| !value.project_paths.is_empty()) {
                let shadow = revision
                    .project_paths
                    .iter()
                    .any(|path| is_shadow_project_path(path));
                let memory_table = if shadow { "shadow_memories" } else { "mc_memories" };
                let mutation_table = if shadow {
                    "shadow_memory_mutation_log"
                } else {
                    "mc_memory_mutation_log"
                };
                let path_column = if shadow {
                    "shadow_project_path"
                } else {
                    "project_path"
                };
                let placeholders = std::iter::repeat_n("?", revision.project_paths.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let current_memory: i64 = tx.query_row(
                    &format!("SELECT COALESCE(MAX(id), 0) FROM {memory_table} WHERE {path_column} IN ({placeholders})"),
                    rusqlite::params_from_iter(revision.project_paths.iter()),
                    |row| row.get(0),
                )?;
                let current_mutation: i64 = tx.query_row(
                    &format!("SELECT COALESCE(MAX(id), 0) FROM {mutation_table} WHERE {path_column} IN ({placeholders})"),
                    rusqlite::params_from_iter(revision.project_paths.iter()),
                    |row| row.get(0),
                )?;
                if current_memory != revision.max_memory_id
                    || current_mutation != revision.mutation_cursor
                {
                    return Ok(CommitOutcome::CasConflict(current.max(0) as u64));
                }
            }

            // INSERT-or-UPDATE in the same fenced txn (bootstrap has no row to UPDATE).
            tx.execute(
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta",
                params![session_id, next as i64, core_json, meta_json],
            )?;
            for drop_id in consumed_drop_ids {
                tx.execute(
                    "DELETE FROM pending_agent_drops WHERE session_id = ?1 AND id = ?2",
                    params![session_id, drop_id],
                )?;
            }
            Ok(CommitOutcome::Committed(next))
        })?;

        match outcome {
            CommitOutcome::Committed(v) => Ok(v),
            CommitOutcome::CasConflict(found) => Err(McStoreError::CasConflict { expected, found }),
        }
    }

    /// Apply a shadow state mirror update in the same fenced transaction that advances
    /// the shadow sequence. The generation and sequence checks run inside the transaction
    /// before any mirror row is written, so a dropped/retried sync cannot partially apply.
    pub fn apply_shadow_state_sync(
        &self,
        request: ShadowStateSyncRequest<'_>,
    ) -> Result<ShadowStateSyncResult, ShadowStateSyncError> {
        let default_core_json = serde_json::to_string(&CoreState::default())
            .map_err(|e| ShadowStateSyncError::Serde(e.to_string()))?;
        let outcome = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![request.session_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;

            let (current, mut core, mut meta) = match row {
                Some((row_version, core_state_json, meta_json)) => {
                    let core = match serde_json::from_str::<CoreState>(&core_state_json) {
                        Ok(core) => core,
                        Err(e) => return Ok(ShadowSyncTxnOutcome::Serde(e.to_string())),
                    };
                    let meta = match serde_json::from_str::<ModuleMeta>(&meta_json) {
                        Ok(meta) => meta,
                        Err(e) => return Ok(ShadowSyncTxnOutcome::Serde(e.to_string())),
                    };
                    (row_version, core, meta)
                }
                None => {
                    let core = match serde_json::from_str::<CoreState>(&default_core_json) {
                        Ok(core) => core,
                        Err(e) => return Ok(ShadowSyncTxnOutcome::Serde(e.to_string())),
                    };
                    (NO_ROW, core, ModuleMeta::default())
                }
            };

            if meta.shadow_generation != request.shadow_generation {
                return Ok(ShadowSyncTxnOutcome::GenerationMismatch {
                    found: meta.shadow_generation,
                });
            }
            if meta.shadow_seq != request.expected_shadow_seq {
                return Ok(ShadowSyncTxnOutcome::SeqMismatch {
                    found: meta.shadow_seq,
                });
            }

            if let Some(declared) = request.seed_boundary_id {
                let adoption = match validated_seed_boundary(declared, request.compartments) {
                    Ok(adoption) => adoption,
                    Err(detail) => {
                        return Ok(ShadowSyncTxnOutcome::InvalidSeedBoundary {
                            declared: declared.to_string(),
                            detail,
                        })
                    }
                };
                core.boundary_id = adoption.boundary_id;
                core.reconcile_pending = false;
                meta.coverage_ordinal = Some(adoption.coverage_end_ordinal);
                meta.coverage_start_ordinal = Some(adoption.coverage_start_ordinal);
                meta.coverage_compartment_seq = Some(adoption.max_sequence);
                meta.folded_compartment_seq = adoption.max_sequence;
                meta.pending_rewrite = None;
            }

            for compartment in request.compartments {
                upsert_compartment_tx(tx, request.session_id, compartment)?;
            }
            replace_shadow_workspace_tx(tx, request.shadow_project_path, request.workspace)?;
            replace_shadow_memories_tx(tx, request.memories)?;
            replace_shadow_memory_mutations_tx(tx, request.memory_mutations)?;

            meta.last_todo_state = request.last_todo_state.clone();
            meta.shadow_seq = meta.shadow_seq.saturating_add(1);
            meta.shadow_acked_watermarks = request.acked_watermarks.clone();

            let next = current.max(0) as u64 + 1;
            let core_json = match serde_json::to_string(&core) {
                Ok(json) => json,
                Err(e) => return Ok(ShadowSyncTxnOutcome::Serde(e.to_string())),
            };
            let meta_json = match serde_json::to_string(&meta) {
                Ok(json) => json,
                Err(e) => return Ok(ShadowSyncTxnOutcome::Serde(e.to_string())),
            };
            tx.execute(
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta",
                params![request.session_id, next as i64, core_json, meta_json],
            )?;

            Ok(ShadowSyncTxnOutcome::Committed(ShadowStateSyncResult {
                shadow_generation: meta.shadow_generation,
                shadow_seq: meta.shadow_seq,
                row_version: next,
            }))
        })?;

        match outcome {
            ShadowSyncTxnOutcome::Committed(result) => Ok(result),
            ShadowSyncTxnOutcome::GenerationMismatch { found } => {
                Err(ShadowStateSyncError::GenerationMismatch {
                    expected: request.shadow_generation,
                    found,
                })
            }
            ShadowSyncTxnOutcome::SeqMismatch { found } => Err(ShadowStateSyncError::SeqMismatch {
                expected: request.expected_shadow_seq,
                found,
            }),
            ShadowSyncTxnOutcome::InvalidSeedBoundary { declared, detail } => {
                Err(ShadowStateSyncError::InvalidSeedBoundary { declared, detail })
            }
            ShadowSyncTxnOutcome::Serde(e) => Err(ShadowStateSyncError::Serde(e)),
        }
    }

    /// Start a new shadow lineage by wiping shadow-owned rows and recreating the cache
    /// state with generation+1, seq=0, and quarantine cleared.
    pub fn reset_shadow_session(
        &self,
        session_id: &str,
        shadow_project_path: &str,
    ) -> Result<ShadowResetResult, McStoreError> {
        let core_json = serde_json::to_string(&CoreState::default())
            .map_err(|e| McStoreError::Serde(e.to_string()))?;
        let result = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            let (current, current_generation) = match row {
                Some((row_version, meta_json)) => {
                    let meta: ModuleMeta = serde_json::from_str(&meta_json)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    (row_version, meta.shadow_generation)
                }
                None => (NO_ROW, 0),
            };
            let mut meta = ModuleMeta {
                shadow_generation: current_generation.saturating_add(1),
                shadow_seq: 0,
                shadow_quarantined: false,
                ..ModuleMeta::default()
            };
            meta.shadow_acked_watermarks = Value::Null;

            tx.execute(
                "DELETE FROM mc_compartments WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM pending_agent_drops WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_reduce_command_ledger WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM shadow_memories
                  WHERE shadow_project_path = ?1
                     OR shadow_project_path IN (
                         SELECT member.project_path
                           FROM mc_workspace_members AS anchor
                           JOIN mc_workspace_members AS member
                             ON member.workspace_id = anchor.workspace_id
                          WHERE anchor.project_path = ?1
                     )",
                params![shadow_project_path],
            )?;
            tx.execute(
                "DELETE FROM shadow_memory_mutation_log
                  WHERE shadow_project_path = ?1
                     OR shadow_project_path IN (
                         SELECT member.project_path
                           FROM mc_workspace_members AS anchor
                           JOIN mc_workspace_members AS member
                             ON member.workspace_id = anchor.workspace_id
                          WHERE anchor.project_path = ?1
                     )",
                params![shadow_project_path],
            )?;
            tx.execute(
                "DELETE FROM mc_workspaces
                  WHERE id IN (
                      SELECT workspace_id FROM mc_workspace_members WHERE project_path = ?1
                  )",
                params![shadow_project_path],
            )?;
            let next = current.max(0) as u64 + 1;
            let meta_json = serde_json::to_string(&meta)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute(
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta",
                params![session_id, next as i64, core_json, meta_json],
            )?;
            Ok(ShadowResetResult {
                shadow_generation: meta.shadow_generation,
                shadow_seq: meta.shadow_seq,
                row_version: next,
            })
        })?;
        Ok(result)
    }

    /// Stores one shadow divergence report and optionally marks the session quarantined in
    /// a single compare-and-swap update. Once quarantined, later reports only increment the
    /// durable pass counter so the first terminal row remains the sole finding. If the
    /// generation no longer matches, the write fails so an older report cannot affect a
    /// newer shadow lineage.
    pub fn record_shadow_divergence(
        &self,
        record: ShadowDivergenceRecord<'_>,
    ) -> Result<ShadowDivergenceWriteResult, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![record.session_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((current, core_json, meta_json)) = row else {
                return Ok(ShadowDivergenceTxnOutcome::GenerationMismatch { found: 0 });
            };
            let mut meta: ModuleMeta = match serde_json::from_str(&meta_json) {
                Ok(meta) => meta,
                Err(e) => return Ok(ShadowDivergenceTxnOutcome::Serde(e.to_string())),
            };
            if meta.shadow_generation != record.shadow_generation {
                return Ok(ShadowDivergenceTxnOutcome::GenerationMismatch {
                    found: meta.shadow_generation,
                });
            }

            if meta.shadow_quarantined {
                meta.shadow_quarantined_pass_count =
                    meta.shadow_quarantined_pass_count.saturating_add(1);
                let next = current.max(0) as u64 + 1;
                let meta_json = match serde_json::to_string(&meta) {
                    Ok(json) => json,
                    Err(e) => return Ok(ShadowDivergenceTxnOutcome::Serde(e.to_string())),
                };
                tx.execute(
                    "UPDATE mc_cache_state SET row_version = ?2, core_state = ?3, meta = ?4
                     WHERE session_id = ?1 AND row_version = ?5",
                    params![record.session_id, next as i64, core_json, meta_json, current],
                )?;
                return Ok(ShadowDivergenceTxnOutcome::Committed(
                    ShadowDivergenceWriteResult {
                        quarantined: true,
                        row_version: next,
                    },
                ));
            }

            tx.execute(
                "INSERT INTO shadow_divergences
                   (session_id, pass_seq, class, first_mid, first_block, first_field,
                    ts_prefix, rs_prefix, first_diff_offset, ts_window, rs_window,
                    normalizations, ts_decision, rs_decision, state_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    record.session_id,
                    record.pass_seq as i64,
                    record.class,
                    record.first_mid,
                    record.first_block,
                    record.first_field,
                    record.ts_prefix,
                    record.rs_prefix,
                    record.first_diff_offset.map(|offset| offset as i64),
                    record.ts_window,
                    record.rs_window,
                    record.normalizations_json,
                    record.ts_decision_json,
                    record.rs_decision_json,
                    record.state_hash,
                    record.created_at_ms,
                ],
            )?;

            let mut next = current.max(0) as u64;
            if record.quarantine && !meta.shadow_quarantined {
                meta.shadow_quarantined = true;
                next = next.saturating_add(1);
                let meta_json = match serde_json::to_string(&meta) {
                    Ok(json) => json,
                    Err(e) => return Ok(ShadowDivergenceTxnOutcome::Serde(e.to_string())),
                };
                tx.execute(
                    "UPDATE mc_cache_state SET row_version = ?2, core_state = ?3, meta = ?4
                     WHERE session_id = ?1 AND row_version = ?5",
                    params![record.session_id, next as i64, core_json, meta_json, current],
                )?;
            }

            Ok(ShadowDivergenceTxnOutcome::Committed(
                ShadowDivergenceWriteResult {
                    quarantined: meta.shadow_quarantined,
                    row_version: next,
                },
            ))
        })?;

        match outcome {
            ShadowDivergenceTxnOutcome::Committed(result) => Ok(result),
            ShadowDivergenceTxnOutcome::GenerationMismatch { found } => {
                Err(McStoreError::CasConflict {
                    expected: Some(record.shadow_generation),
                    found,
                })
            }
            ShadowDivergenceTxnOutcome::Serde(e) => Err(McStoreError::Serde(e)),
        }
    }

    pub fn load_shadow_divergences(
        &self,
        session_id: &str,
    ) -> Result<Vec<ShadowDivergenceRow>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, pass_seq, class, first_mid, first_block, first_field,
                        ts_prefix, rs_prefix, first_diff_offset, ts_window, rs_window,
                        normalizations, ts_decision, rs_decision, state_hash, created_at
                   FROM shadow_divergences
                  WHERE session_id = ?1
                  ORDER BY pass_seq ASC, id ASC",
            )?;
            let mapped = stmt
                .query_map(params![session_id], |r| {
                    Ok(ShadowDivergenceRow {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        pass_seq: r.get::<_, i64>(2)?.max(0) as u64,
                        class: r.get(3)?,
                        first_mid: r.get(4)?,
                        first_block: r.get(5)?,
                        first_field: r.get(6)?,
                        ts_prefix: r.get(7)?,
                        rs_prefix: r.get(8)?,
                        first_diff_offset: r
                            .get::<_, Option<i64>>(9)?
                            .map(|offset| offset.max(0) as u64),
                        ts_window: r.get(10)?,
                        rs_window: r.get(11)?,
                        normalizations_json: r.get(12)?,
                        ts_decision_json: r.get(13)?,
                        rs_decision_json: r.get(14)?,
                        state_hash: r.get(15)?,
                        created_at_ms: r.get(16)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Read a session's compartments in chronological order (oldest first), the order
    /// the decay renderer expects (it indexes from newest internally).
    pub fn load_compartments(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredCompartment>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence, start_message, end_message, start_message_id, end_message_id,
                        start_date, end_date, title, content, p1, p2, p3, p4, importance,
                        episode_type, legacy, created_at
                 FROM mc_compartments WHERE session_id = ?1 ORDER BY sequence ASC",
            )?;
            let mapped = stmt
                .query_map(params![session_id], |r| {
                    Ok(StoredCompartment {
                        sequence: r.get(0)?,
                        start_message: r.get(1)?,
                        end_message: r.get(2)?,
                        start_message_id: r.get(3)?,
                        end_message_id: r.get(4)?,
                        start_date: r.get(5)?,
                        end_date: r.get(6)?,
                        title: r.get(7)?,
                        content: r.get(8)?,
                        p1: r.get(9)?,
                        p2: r.get(10)?,
                        p3: r.get(11)?,
                        p4: r.get(12)?,
                        importance: r.get::<_, Option<i64>>(13)?.unwrap_or(50) as i32,
                        episode_type: r.get(14)?,
                        legacy: r.get::<_, Option<i64>>(15)?.unwrap_or(0) as i32,
                        created_at: r.get(16)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Read the compartment rows and session revert epoch in one store snapshot for
    /// historian assembly. The epoch is the fence carried by the firing until publish.
    pub fn load_historian_assembly_snapshot(
        &self,
        session_id: &str,
    ) -> Result<HistorianAssemblySnapshot, McStoreError> {
        let (meta_json, compartments) = self.inner.with_conn(|conn| {
            let meta_json = conn
                .query_row(
                    "SELECT meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            let mut stmt = conn.prepare(
                "SELECT sequence, start_message, end_message, start_message_id, end_message_id,
                        start_date, end_date, title, content, p1, p2, p3, p4, importance,
                        episode_type, legacy, created_at
                 FROM mc_compartments WHERE session_id = ?1 ORDER BY sequence ASC",
            )?;
            let compartments = stmt
                .query_map(params![session_id], |r| {
                    Ok(StoredCompartment {
                        sequence: r.get(0)?,
                        start_message: r.get(1)?,
                        end_message: r.get(2)?,
                        start_message_id: r.get(3)?,
                        end_message_id: r.get(4)?,
                        start_date: r.get(5)?,
                        end_date: r.get(6)?,
                        title: r.get(7)?,
                        content: r.get(8)?,
                        p1: r.get(9)?,
                        p2: r.get(10)?,
                        p3: r.get(11)?,
                        p4: r.get(12)?,
                        importance: r.get::<_, Option<i64>>(13)?.unwrap_or(50) as i32,
                        episode_type: r.get(14)?,
                        legacy: r.get::<_, Option<i64>>(15)?.unwrap_or(0) as i32,
                        created_at: r.get(16)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((meta_json, compartments))
        })?;
        let revert_epoch = match meta_json {
            Some(json) => {
                serde_json::from_str::<ModuleMeta>(&json)
                    .map_err(|e| McStoreError::Serde(e.to_string()))?
                    .revert_epoch
            }
            None => 0,
        };
        Ok(HistorianAssemblySnapshot {
            compartments,
            revert_epoch,
        })
    }

    /// The highest compartment `sequence` for a session (0 when none). A cheap read the
    /// transform does every pass to detect "a new compartment was published" without
    /// loading the full compartment rows (those load only on the pass that actually
    /// re-composes the m1 delta block).
    pub fn max_compartment_seq(&self, session_id: &str) -> Result<i64, McStoreError> {
        let max = self.inner.with_conn(|conn| {
            let v: i64 = conn.query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM mc_compartments WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            Ok(v)
        })?;
        Ok(max)
    }

    /// Whether the session has any compartment at all. This is a presence check, NOT a
    /// count or a max-sequence read: `max_compartment_seq` COALESCEs a missing MAX to 0,
    /// which is indistinguishable from a real first compartment at sequence 0, so it
    /// cannot answer "does a compartment exist". The first-fold HARD trigger needs the
    /// unambiguous existence answer (empty boundary + a compartment present => the first
    /// fold is due), so this returns a true/false from `SELECT EXISTS` on the session
    /// index — O(1), never touches the sequence value.
    pub fn has_compartments(&self, session_id: &str) -> Result<bool, McStoreError> {
        let exists = self.inner.with_conn(|conn| {
            let v: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM mc_compartments WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )?;
            Ok(v)
        })?;
        Ok(exists != 0)
    }

    /// Replace a session's entire compartment set in one fenced transaction. The
    /// history producer republishes the full chronological set each time, so a
    /// wholesale delete-then-insert (rather than an incremental upsert) keeps the
    /// stored `sequence` contiguous. Writes are serialized by the store's single-writer
    /// lease (the same one guarding the cache-state commit).
    pub fn replace_compartments(
        &self,
        session_id: &str,
        compartments: &[StoredCompartment],
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "DELETE FROM mc_chunk_transcripts WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_compartments WHERE session_id = ?1",
                params![session_id],
            )?;
            for c in compartments {
                insert_compartment_tx(tx, session_id, c.sequence, c)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Delete every compartment after `keep_through_seq` and bump the session revert
    /// epoch under the same row-version CAS. A no-op truncation returns the current
    /// epoch/version without rewriting the meta blob.
    pub fn truncate_compartments_for_revert(
        &self,
        session_id: &str,
        keep_through_seq: i64,
        expected_row_version: Option<u64>,
    ) -> Result<TruncateOutcome, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((current, meta_json)) = row else {
                return Ok(TruncateTxnOutcome::CasConflict(0));
            };

            let cas_ok = match expected_row_version {
                Some(v) => current == v as i64,
                None => current == NO_ROW,
            };
            if !cas_ok {
                return Ok(TruncateTxnOutcome::CasConflict(current.max(0) as u64));
            }

            let mut meta: ModuleMeta = match serde_json::from_str(&meta_json) {
                Ok(meta) => meta,
                Err(e) => return Ok(TruncateTxnOutcome::Serde(e.to_string())),
            };

            let (dropped_count, dropped_min, dropped_max): (i64, Option<i64>, Option<i64>) = tx
                .query_row(
                    "SELECT COUNT(*), MIN(sequence), MAX(sequence)
                     FROM mc_compartments WHERE session_id = ?1 AND sequence > ?2",
                    params![session_id, keep_through_seq],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
            if dropped_count == 0 {
                return Ok(TruncateTxnOutcome::Committed(TruncateOutcome {
                    revert_epoch: meta.revert_epoch,
                    last_recut: meta.last_recut,
                    row_version: current.max(0) as u64,
                }));
            }

            let surviving_tail = tx
                .query_row(
                    "SELECT sequence, end_message_id FROM mc_compartments
                     WHERE session_id = ?1 AND sequence <= ?2
                     ORDER BY sequence DESC LIMIT 1",
                    params![session_id, keep_through_seq],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            let surviving_head_id = tx
                .query_row(
                    "SELECT start_message_id FROM mc_compartments
                     WHERE session_id = ?1 AND sequence <= ?2
                     ORDER BY sequence ASC LIMIT 1",
                    params![session_id, keep_through_seq],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;

            let next_epoch = meta.revert_epoch.saturating_add(1);
            let dropped_range = match (dropped_min, dropped_max) {
                (Some(min), Some(max)) if min == max => min.to_string(),
                (Some(min), Some(max)) => format!("{min}..{max}"),
                _ => "unknown".to_string(),
            };
            let surviving_seq = surviving_tail
                .as_ref()
                .map(|(seq, _)| seq.to_string())
                .unwrap_or_else(|| "none".to_string());
            let live_head = surviving_head_id.unwrap_or_else(|| "none".to_string());
            let live_tail = surviving_tail
                .as_ref()
                .map(|(_, id)| id.clone())
                .unwrap_or_else(|| "none".to_string());
            let last_recut = Some(format!(
                "dropped seq {dropped_range}; surviving seq {surviving_seq}; live head {live_head}; live tail {live_tail}; epoch {next_epoch}"
            ));
            meta.revert_epoch = next_epoch;
            meta.last_recut = last_recut.clone();
            let meta_json = match serde_json::to_string(&meta) {
                Ok(json) => json,
                Err(e) => return Ok(TruncateTxnOutcome::Serde(e.to_string())),
            };

            tx.execute(
                "DELETE FROM mc_chunk_transcripts WHERE session_id = ?1 AND compartment_seq > ?2",
                params![session_id, keep_through_seq],
            )?;
            tx.execute(
                "DELETE FROM mc_compartments WHERE session_id = ?1 AND sequence > ?2",
                params![session_id, keep_through_seq],
            )?;
            let next = current as u64 + 1;
            tx.execute(
                "UPDATE mc_cache_state SET row_version = ?2, meta = ?3
                 WHERE session_id = ?1 AND row_version = ?4",
                params![session_id, next as i64, meta_json, current],
            )?;

            Ok(TruncateTxnOutcome::Committed(TruncateOutcome {
                revert_epoch: next_epoch,
                last_recut,
                row_version: next,
            }))
        })?;

        match outcome {
            TruncateTxnOutcome::Committed(outcome) => Ok(outcome),
            TruncateTxnOutcome::CasConflict(found) => Err(McStoreError::CasConflict {
                expected: expected_row_version,
                found,
            }),
            TruncateTxnOutcome::Serde(e) => Err(McStoreError::Serde(e)),
        }
    }

    /// Append compartments at the current tail without renumbering existing rows.
    /// The incoming `sequence` values are treated as producer-local hints; durable
    /// sequences are assigned contiguously after the current max so concurrent readers
    /// never observe gaps or rewritten history.
    pub fn append_compartments(
        &self,
        session_id: &str,
        compartments: &[StoredCompartment],
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            append_compartments_tx(tx, session_id, compartments)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Promote validated historian facts into project memories using exact-content
    /// de-duplication against the active render set. This path is additive only: it
    /// inserts new `mc_memories` rows and never writes mutation-log rows, so the next
    /// m1/materialization pass observes the rows solely through the max-memory-id
    /// watermark.
    pub fn promote_facts(
        &self,
        project_path: &str,
        facts: &[FactCandidate],
    ) -> Result<Vec<PromotedRef>, McStoreError> {
        let promoted = self
            .inner
            .with_conn_fenced(|tx| promote_facts_tx(tx, project_path, facts))?;
        Ok(promoted)
    }

    /// Load a complete memory row by id. This is the guard-layer read: unlike the render
    /// projection it includes ownership, lifecycle, merge lineage, and metadata columns.
    pub fn get_memory_full(&self, id: i64) -> Result<Option<StoredMemoryFull>, McStoreError> {
        let row = self.inner.with_conn(|conn| {
            conn.query_row(
                MEMORY_FULL_SELECT_BY_ID,
                params![id],
                stored_memory_full_from_row,
            )
            .optional()
        })?;
        Ok(row)
    }

    /// Insert a memory row unless an existing row already matches the project, category,
    /// and normalized content hash. Duplicate hits update only bookkeeping fields such as
    /// `seen_count` and timestamps, and skip the mutation log because the rendered content
    /// did not change.
    pub fn insert_memory(&self, input: InsertMemoryInput<'_>) -> Result<i64, McStoreError> {
        let memory_id = self.inner.with_conn_fenced(|tx| {
            let normalized_hash = compute_normalized_memory_hash(input.content);
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM mc_memories
                     WHERE project_path = ?1 AND category = ?2 AND normalized_hash = ?3",
                    params![input.project_path, input.category, normalized_hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                tx.execute(
                    "UPDATE mc_memories
                        SET seen_count = COALESCE(seen_count, 0) + 1,
                            last_seen_at = ?1,
                            updated_at = ?1
                      WHERE id = ?2",
                    params![input.now_ms, id],
                )?;
                return Ok(id);
            }

            tx.execute(
                "INSERT INTO mc_memories
                   (project_path, category, content, normalized_hash, importance,
                    source_session_id, source_type, seen_count, retrieval_count,
                    first_seen_at, created_at, updated_at, last_seen_at, status,
                    expires_at, verification_status, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 0, ?8, ?8, ?8, ?8,
                         'active', ?9, 'unverified', ?10)",
                params![
                    input.project_path,
                    input.category,
                    input.content,
                    normalized_hash,
                    input.importance.map(i64::from),
                    input.source_session_id,
                    input.source_type.unwrap_or("historian"),
                    input.now_ms,
                    input.expires_at,
                    input.metadata_json,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        Ok(memory_id)
    }

    /// Replace an owned primary memory's content and append its cache-visible mutation in
    /// the same fenced transaction. Shared workspace visibility is read-only for primary
    /// agents, so project ownership and lifecycle are rechecked after the transaction begins.
    pub fn update_memory_content(
        &self,
        project_path: &str,
        id: i64,
        content: &str,
        now_ms: i64,
    ) -> Result<Option<StoredMemoryFull>, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let Some(memory) = load_memory_full_tx(tx, id)? else {
                return Ok(MemoryMutationOutcome::NotFound);
            };
            if memory.project_path != project_path
                || memory.superseded_by_memory_id.is_some()
                || !matches!(memory.status.as_str(), "active" | "permanent")
            {
                return Ok(MemoryMutationOutcome::NotFound);
            }
            let normalized_hash = compute_normalized_memory_hash(content);
            let duplicate_id = tx
                .query_row(
                    "SELECT id FROM mc_memories
                      WHERE project_path = ?1 AND category = ?2 AND normalized_hash = ?3
                      LIMIT 1",
                    params![project_path, memory.category, normalized_hash],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(duplicate_id) = duplicate_id.filter(|duplicate_id| *duplicate_id != id) {
                return Ok(MemoryMutationOutcome::Duplicate(duplicate_id));
            }
            tx.execute(
                "UPDATE mc_memories
                    SET content = ?1,
                        normalized_hash = ?2,
                        updated_at = ?3,
                        shareable = 0,
                        classified_at = NULL
                  WHERE id = ?4",
                params![content, normalized_hash, now_ms, id],
            )?;
            append_memory_mutation_tx(
                tx,
                MemoryMutationAppend {
                    project_path: &memory.project_path,
                    mutation_type: "update",
                    target_memory_id: id,
                    superseded_by_id: None,
                    category: Some(&memory.category),
                    new_content: Some(content),
                    queued_at: now_ms,
                },
            )?;
            Ok(MemoryMutationOutcome::Applied(Box::new(
                load_memory_full_tx(tx, id)?,
            )))
        })?;
        match outcome {
            MemoryMutationOutcome::NotFound => Ok(None),
            MemoryMutationOutcome::Applied(row) => Ok(*row),
            MemoryMutationOutcome::Duplicate(id) => {
                Err(McStoreError::MemoryDuplicateContent { id })
            }
        }
    }

    /// Archive one owned memory. Project ownership and lifecycle are checked inside the
    /// fenced transaction; an already archived row remains an idempotent success.
    pub fn archive_memory(
        &self,
        project_path: &str,
        id: i64,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<Option<StoredMemoryFull>, McStoreError> {
        let Some(_) = self.archive_memories(project_path, &[id], reason, now_ms)? else {
            return Ok(None);
        };
        self.get_memory_full(id)
    }

    /// Validate and archive an entire owned batch in one fenced transaction.
    pub fn archive_memories(
        &self,
        project_path: &str,
        ids: &[i64],
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<Option<Vec<i64>>, McStoreError> {
        self.inner
            .with_conn_fenced(|tx| {
                let mut memories = Vec::with_capacity(ids.len());
                for id in ids {
                    let Some(memory) = load_memory_full_tx(tx, *id)? else {
                        return Ok(None);
                    };
                    if memory.project_path != project_path
                        || memory.superseded_by_memory_id.is_some()
                        || !matches!(memory.status.as_str(), "active" | "permanent" | "archived")
                    {
                        return Ok(None);
                    }
                    memories.push(memory);
                }

                let trimmed_reason = reason.map(str::trim).filter(|value| !value.is_empty());
                let mut archived = Vec::new();
                for memory in memories {
                    if memory.status == "archived" {
                        continue;
                    }
                    if let Some(reason) = trimmed_reason {
                        let metadata_json =
                            merge_archive_reason(memory.metadata_json.as_deref(), reason);
                        tx.execute(
                            "UPDATE mc_memories
                                SET status = 'archived', metadata_json = ?1, updated_at = ?2
                              WHERE id = ?3",
                            params![metadata_json, now_ms, memory.id],
                        )?;
                    } else {
                        tx.execute(
                            "UPDATE mc_memories SET status = 'archived', updated_at = ?1 WHERE id = ?2",
                            params![now_ms, memory.id],
                        )?;
                    }
                    append_memory_mutation_tx(
                        tx,
                        MemoryMutationAppend {
                            project_path: &memory.project_path,
                            mutation_type: "archive",
                            target_memory_id: memory.id,
                            superseded_by_id: None,
                            category: None,
                            new_content: None,
                            queued_at: now_ms,
                        },
                    )?;
                    archived.push(memory.id);
                }
                Ok(Some(archived))
            })
            .map_err(McStoreError::from)
    }

    /// Merge owned primary source memories into an owned primary target in one fenced
    /// transaction. Ownership, lifecycle, disjointness, and category equality are rechecked
    /// while locked so concurrent merges cannot rewrite an established lineage.
    pub fn merge_memories(
        &self,
        project_path: &str,
        target_id: i64,
        source_ids: &[i64],
        merged_content: &str,
        now_ms: i64,
    ) -> Result<Option<StoredMemoryFull>, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let Some(target) = load_memory_full_tx(tx, target_id)? else {
                return Ok(MemoryMutationOutcome::NotFound);
            };
            if target.project_path != project_path
                || target.superseded_by_memory_id.is_some()
                || !matches!(target.status.as_str(), "active" | "permanent")
            {
                return Ok(MemoryMutationOutcome::NotFound);
            }

            let mut unique_sources: Vec<i64> = source_ids.to_vec();
            unique_sources.sort_unstable();
            unique_sources.dedup();
            if unique_sources.is_empty()
                || unique_sources.len() != source_ids.len()
                || unique_sources.binary_search(&target_id).is_ok()
            {
                return Ok(MemoryMutationOutcome::NotFound);
            }

            let mut source_rows = Vec::with_capacity(unique_sources.len());
            for source_id in unique_sources {
                let Some(source) = load_memory_full_tx(tx, source_id)? else {
                    return Ok(MemoryMutationOutcome::NotFound);
                };
                if source.project_path != project_path
                    || source.category != target.category
                    || source.superseded_by_memory_id.is_some()
                    || !matches!(source.status.as_str(), "active" | "permanent")
                {
                    return Ok(MemoryMutationOutcome::NotFound);
                }
                source_rows.push(source);
            }

            let normalized_hash = compute_normalized_memory_hash(merged_content);
            let duplicate_id = tx
                .query_row(
                    "SELECT id FROM mc_memories
                      WHERE project_path = ?1 AND category = ?2 AND normalized_hash = ?3
                      LIMIT 1",
                    params![project_path, target.category, normalized_hash],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(duplicate_id) =
                duplicate_id.filter(|duplicate_id| *duplicate_id != target_id)
            {
                return Ok(MemoryMutationOutcome::Duplicate(duplicate_id));
            }

            let mut affected = Vec::with_capacity(source_rows.len() + 1);
            affected.push(target.clone());
            affected.extend(source_rows.iter().cloned());
            let merged_from = merged_from_json(&affected);
            let seen_count: i64 = affected.iter().map(|memory| memory.seen_count.max(0)).sum();
            let retrieval_count: i64 = affected
                .iter()
                .map(|memory| memory.retrieval_count.max(0))
                .sum();
            let merged_status = if affected.iter().any(|memory| memory.status == "permanent") {
                "permanent"
            } else {
                "active"
            };

            for source in &source_rows {
                tx.execute(
                    "UPDATE mc_memories
                        SET status = 'archived',
                            superseded_by_memory_id = ?1,
                            updated_at = ?2
                      WHERE id = ?3",
                    params![target_id, now_ms, source.id],
                )?;
                append_memory_mutation_tx(
                    tx,
                    MemoryMutationAppend {
                        project_path: &source.project_path,
                        mutation_type: "superseded",
                        target_memory_id: source.id,
                        superseded_by_id: Some(target_id),
                        category: None,
                        new_content: None,
                        queued_at: now_ms,
                    },
                )?;
            }

            tx.execute(
                "UPDATE mc_memories
                    SET content = ?1,
                        normalized_hash = ?2,
                        seen_count = ?3,
                        retrieval_count = ?4,
                        merged_from = ?5,
                        status = ?6,
                        updated_at = ?7,
                        shareable = 0,
                        classified_at = NULL
                  WHERE id = ?8",
                params![
                    merged_content,
                    normalized_hash,
                    seen_count,
                    retrieval_count,
                    merged_from,
                    merged_status,
                    now_ms,
                    target_id,
                ],
            )?;
            append_memory_mutation_tx(
                tx,
                MemoryMutationAppend {
                    project_path: &target.project_path,
                    mutation_type: "update",
                    target_memory_id: target_id,
                    superseded_by_id: None,
                    category: Some(&target.category),
                    new_content: Some(merged_content),
                    queued_at: now_ms,
                },
            )?;

            Ok(MemoryMutationOutcome::Applied(Box::new(
                load_memory_full_tx(tx, target_id)?,
            )))
        })?;
        match outcome {
            MemoryMutationOutcome::NotFound => Ok(None),
            MemoryMutationOutcome::Applied(row) => Ok(*row),
            MemoryMutationOutcome::Duplicate(id) => {
                Err(McStoreError::MemoryDuplicateContent { id })
            }
        }
    }

    /// Publish a validated historian chunk in one CAS-gated transaction. The publish
    /// predicate proves the producer still matches the exact firing that created the
    /// chunk; stale reattaches or a second racing publisher fail before any rows are
    /// appended. The transaction intentionally leaves render state (`CoreState`,
    /// `coverage_ordinal`, watermarks, and m1 revision) untouched: new rows become
    /// visible only through the existing store watermarks on a later materializing pass.
    pub fn publish_historian_chunk(
        &self,
        request: HistorianPublishRequest<'_>,
    ) -> Result<HistorianPublishResult, HistorianPublishError> {
        let session_id = request.session_id;
        let expected_row_version = request.expected_row_version;
        let predicate = request.predicate;
        let outcome = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;

            let Some((current, meta_json)) = row else {
                return Ok(PublishTxnOutcome::InvalidState("missing".to_string()));
            };

            let cas_ok = match expected_row_version {
                Some(v) => current == v as i64,
                None => current == NO_ROW,
            };
            if !cas_ok {
                return Ok(PublishTxnOutcome::CasConflict {
                    found: current.max(0) as u64,
                    reason: None,
                });
            }

            let mut meta: ModuleMeta = match serde_json::from_str(&meta_json) {
                Ok(meta) => meta,
                Err(e) => return Ok(PublishTxnOutcome::Serde(e.to_string())),
            };

            if !matches!(
                meta.historian.state,
                HistorianPhase::Publishing | HistorianPhase::AwaitingProducer
            ) {
                return Ok(PublishTxnOutcome::InvalidState(
                    meta.historian.state.as_str().to_string(),
                ));
            }

            let predicate_matches = meta.historian.firing_seq == predicate.firing_seq
                && meta.historian.producer_run_id.as_deref()
                    == Some(predicate.producer_run_id.as_str())
                && meta.historian.chunk_fingerprint == predicate.chunk_fingerprint;
            if !predicate_matches {
                return Ok(PublishTxnOutcome::StateMismatch(meta.historian));
            }

            if meta.revert_epoch != request.expected_revert_epoch {
                return Ok(PublishTxnOutcome::CasConflict {
                    found: current.max(0) as u64,
                    reason: Some(
                        "revert epoch mismatch (session was re-cut mid-firing)".to_string(),
                    ),
                });
            }

            let first_appended_sequence = next_compartment_sequence_tx(tx, session_id)?;
            append_compartments_tx(tx, session_id, request.compartments)?;
            if let Some(transcript) = request.chunk_transcript {
                insert_chunk_transcripts_tx(
                    tx,
                    session_id,
                    first_appended_sequence,
                    request.compartments,
                    transcript,
                )?;
            }
            let promoted_refs = promote_facts_tx(tx, request.project_path, request.facts)?;

            meta.publication_floor_ordinal = Some(
                meta.publication_floor_ordinal
                    .unwrap_or(1)
                    .max(request.publication_floor_ordinal.max(1)),
            );
            meta.historian = idle_historian_after_success(meta.historian.firing_seq);

            let next = current as u64 + 1;
            let meta_json = match serde_json::to_string(&meta) {
                Ok(json) => json,
                Err(e) => return Ok(PublishTxnOutcome::Serde(e.to_string())),
            };
            tx.execute(
                "UPDATE mc_cache_state SET row_version = ?2, meta = ?3
                 WHERE session_id = ?1 AND row_version = ?4",
                params![session_id, next as i64, meta_json, current],
            )?;

            Ok(PublishTxnOutcome::Committed(HistorianPublishResult {
                row_version: next,
                promoted_refs,
            }))
        })?;

        match outcome {
            PublishTxnOutcome::Committed(result) => Ok(result),
            PublishTxnOutcome::CasConflict { found, reason } => {
                Err(HistorianPublishError::CasConflict {
                    expected: expected_row_version,
                    found,
                    reason,
                })
            }
            PublishTxnOutcome::StateMismatch(found) => Err(HistorianPublishError::StateMismatch {
                expected: Box::new(predicate.clone()),
                found: Box::new(found),
            }),
            PublishTxnOutcome::InvalidState(state) => {
                Err(HistorianPublishError::InvalidState { state })
            }
            PublishTxnOutcome::Serde(e) => Err(HistorianPublishError::Serde(e)),
        }
    }

    /// Load a project's render-eligible memories: `active` + `permanent`, excluding
    /// expired ones (an `expires_at` at/before `now_ms`, NULL = never expires), ordered
    /// by importance descending then id ascending (the budget-trim order — highest
    /// importance survives a trim; id breaks ties deterministically). The expiry cutoff
    /// is supplied by the caller, NOT read from the live clock, so the full render and
    /// every later byte-identical replay of it observe the SAME memory set — a live
    /// clock would expire a memory mid-replay and silently change the rendered bytes.
    pub fn load_active_memories(
        &self,
        project_path: &str,
        now_ms: i64,
    ) -> Result<Vec<StoredMemory>, McStoreError> {
        if is_shadow_project_path(project_path) {
            return self.load_active_shadow_memories(project_path, now_ms);
        }
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_path, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                 FROM mc_memories
                 WHERE project_path = ?1
                   AND status IN ('active', 'permanent')
                   AND (expires_at IS NULL OR expires_at > ?2)
                 ORDER BY COALESCE(importance, 50) DESC, id ASC",
            )?;
            let mapped = stmt
                .query_map(params![project_path, now_ms], |r| {
                    Ok(StoredMemory {
                        id: r.get(0)?,
                        project_path: r.get(1)?,
                        category: r.get(2)?,
                        content: r.get(3)?,
                        importance: r.get(4)?,
                        status: r.get(5)?,
                        expires_at: r.get(6)?,
                        superseded_by_memory_id: r.get(7)?,
                        updated_at: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    fn load_active_shadow_memories(
        &self,
        shadow_project_path: &str,
        now_ms: i64,
    ) -> Result<Vec<StoredMemory>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, shadow_project_path, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                   FROM shadow_memories
                  WHERE shadow_project_path = ?1
                    AND status IN ('active', 'permanent')
                    AND (expires_at IS NULL OR expires_at > ?2)
                  ORDER BY COALESCE(importance, 50) DESC, id ASC",
            )?;
            let mapped = stmt
                .query_map(params![shadow_project_path, now_ms], |r| {
                    Ok(StoredMemory {
                        id: r.get(0)?,
                        project_path: r.get(1)?,
                        category: r.get(2)?,
                        content: r.get(3)?,
                        importance: r.get(4)?,
                        status: r.get(5)?,
                        expires_at: r.get(6)?,
                        superseded_by_memory_id: r.get(7)?,
                        updated_at: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// The coalesced memory corrections to render as the delta, across one OR MORE
    /// project identities (the workspace union — a single-project session passes a
    /// 1-element slice). Mutation-log rows with `id > after_id`, from any of
    /// `project_paths`, whose `target_memory_id` is in `rendered_memory_ids` (the exact
    /// set of memories included in the last rendered baseline). The manifest membership
    /// IS the share filter: a foreign non-shared memory never entered the baseline, so
    /// it's not in `rendered_memory_ids`, so its mutation can't supersede here — no extra
    /// category check needed. The manifest test (NOT an `id <= last_id` test) is required
    /// because budget-trim can drop a low-importance in-range memory.
    ///
    /// Coalesced to ONE correction per target, deterministic latest-wins with terminal
    /// precedence: a terminal mutation (archive/delete/superseded) always outranks a
    /// later `update`, so an update queued after an archive can't resurrect a memory that
    /// already left the set. Sorted by id for a stable render order. Coalescing by
    /// target_id is union-safe (a memory id is unique across the store).
    pub fn memory_mutations_for_render(
        &self,
        project_paths: &[String],
        after_id: i64,
        rendered_memory_ids: &[i64],
    ) -> Result<Vec<StoredMemoryMutation>, McStoreError> {
        if rendered_memory_ids.is_empty() || project_paths.is_empty() {
            return Ok(Vec::new());
        }
        if project_paths
            .iter()
            .any(|path| is_shadow_project_path(path))
        {
            return self.shadow_memory_mutations_for_render(
                project_paths,
                after_id,
                rendered_memory_ids,
            );
        }
        // dedup + sort the id set for a stable IN-clause.
        let mut ids: Vec<i64> = rendered_memory_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let id_ph = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        // dedup the project identities for a stable IN-clause.
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let proj_ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, mutation_type, target_memory_id, superseded_by_id, category,
                        new_content, queued_at
                 FROM mc_memory_mutation_log
                 WHERE project_path IN ({proj_ph}) AND id > ? AND target_memory_id IN ({id_ph})
                 ORDER BY id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            // bind: the project set, then after_id, then the id set (matching SQL order).
            let mut binds: Vec<rusqlite::types::Value> = projects
                .iter()
                .map(|p| rusqlite::types::Value::from(p.clone()))
                .collect();
            binds.push(rusqlite::types::Value::from(after_id));
            binds.extend(ids.iter().map(|&i| rusqlite::types::Value::from(i)));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok(StoredMemoryMutation {
                        id: r.get(0)?,
                        mutation_type: r.get(1)?,
                        target_memory_id: r.get(2)?,
                        superseded_by_id: r.get(3)?,
                        category: r.get(4)?,
                        new_content: r.get(5)?,
                        queued_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;

        Ok(coalesce_mutations(rows))
    }

    fn shadow_memory_mutations_for_render(
        &self,
        project_paths: &[String],
        after_id: i64,
        rendered_memory_ids: &[i64],
    ) -> Result<Vec<StoredMemoryMutation>, McStoreError> {
        let mut ids: Vec<i64> = rendered_memory_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let id_ph = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut projects: Vec<String> = project_paths
            .iter()
            .filter(|path| is_shadow_project_path(path))
            .cloned()
            .collect();
        projects.sort_unstable();
        projects.dedup();
        if projects.is_empty() {
            return Ok(Vec::new());
        }
        let proj_ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, mutation_type, target_memory_id, superseded_by_id, category,
                        new_content, queued_at
                   FROM shadow_memory_mutation_log
                  WHERE shadow_project_path IN ({proj_ph}) AND id > ? AND target_memory_id IN ({id_ph})
                  ORDER BY id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<rusqlite::types::Value> = projects
                .iter()
                .map(|p| rusqlite::types::Value::from(p.clone()))
                .collect();
            binds.push(rusqlite::types::Value::from(after_id));
            binds.extend(ids.iter().map(|&i| rusqlite::types::Value::from(i)));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok(StoredMemoryMutation {
                        id: r.get(0)?,
                        mutation_type: r.get(1)?,
                        target_memory_id: r.get(2)?,
                        superseded_by_id: r.get(3)?,
                        category: r.get(4)?,
                        new_content: r.get(5)?,
                        queued_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;

        Ok(coalesce_mutations(rows))
    }

    /// Resolve a project's workspace membership: the union of member identities it
    /// reads, the share-category allow-list, and per-foreign-member display attribution.
    /// Returns None when the project is in no workspace (the single-project fast path —
    /// the caller reads only its own memories). A project is in at most one workspace
    /// (the UNIQUE index on project_path).
    pub fn resolve_workspace_membership(
        &self,
        project_path: &str,
    ) -> Result<Option<WorkspaceMembership>, McStoreError> {
        let membership = self.inner.with_conn(|conn| {
            // which workspace (if any) does this project belong to?
            let ws: Option<(i64, String)> = conn
                .query_row(
                    "SELECT w.id, w.share_categories
                       FROM mc_workspace_members m
                       JOIN mc_workspaces w ON w.id = m.workspace_id
                      WHERE m.project_path = ?1",
                    params![project_path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((workspace_id, share_categories_json)) = ws else {
                return Ok(None);
            };

            let mut stmt = conn.prepare(
                "SELECT project_path, display_name FROM mc_workspace_members
                  WHERE workspace_id = ?1 ORDER BY project_path ASC",
            )?;
            let members: Vec<(String, String)> = stmt
                .query_map(params![workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;

            let union_identities: Vec<String> = members.iter().map(|(p, _)| p.clone()).collect();
            let display_name_by_path = members.into_iter().collect();
            let share_categories: Vec<String> =
                serde_json::from_str(&share_categories_json).unwrap_or_default();

            Ok(Some(WorkspaceMembership {
                union_identities,
                own_identity: project_path.to_string(),
                share_categories,
                display_name_by_path,
            }))
        })?;
        Ok(membership)
    }

    /// A DETERMINISTIC fingerprint of the project's workspace membership + share policy.
    /// The cache layer treats a change in this value as a baseline re-render (a HARD fold):
    /// a membership/policy change re-composes m0 over a DIFFERENT project set, so a stale
    /// fingerprint can't be tolerated the way stale content can. MUST be canonical: members
    /// sorted by `project_path`, the share-category list sorted, each field length-prefixed
    /// so no value forges a boundary. A nondeterministic fingerprint over a STABLE
    /// workspace would false-HARD every pass (the over-bust the m0/m1 split exists to
    /// avoid). Empty string when the project is in no workspace (the single-project state —
    /// a stable "no workspace" marker). NOTE: in this slice the fingerprint covers
    /// membership + share-policy only; production also folds each member's project-memory
    /// epoch, which has no `mc_*` source yet (it lands with the deferred
    /// `project_memory_epoch` marker when the write paths relocate).
    pub fn workspace_fingerprint(&self, project_path: &str) -> Result<String, McStoreError> {
        let Some(m) = self.resolve_workspace_membership(project_path)? else {
            return Ok(String::new());
        };
        // resolve_workspace_membership returns members sorted by project_path; sort the
        // share categories too so the policy axis is order-independent.
        let mut shared = m.share_categories.clone();
        shared.sort_unstable();
        let mut out = String::from("ws[");
        for id in &m.union_identities {
            out.push_str(&format!("m:{}:{};", id.len(), id));
        }
        out.push_str("|share:");
        for cat in &shared {
            out.push_str(&format!("{}:{};", cat.len(), cat));
        }
        out.push(']');
        Ok(out)
    }

    /// Load render-eligible memories across a workspace UNION: every member's `active` +
    /// `permanent` non-expired memories, but a FOREIGN member's only in the shared
    /// categories (`share_categories`); the OWN project sees all its own. Shadow
    /// workspaces use the isolated mirror table while following the same visibility path.
    pub fn load_workspace_union_memories(
        &self,
        membership: &WorkspaceMembership,
        now_ms: i64,
    ) -> Result<Vec<StoredMemory>, McStoreError> {
        if membership.union_identities.is_empty() {
            return Ok(Vec::new());
        }
        let shadow = is_shadow_project_path(&membership.own_identity);
        let path_column = if shadow {
            "shadow_project_path"
        } else {
            "project_path"
        };
        let table = if shadow {
            "shadow_memories"
        } else {
            "mc_memories"
        };
        let (sharing, binds) =
            workspace_union_memory_visibility_filter_for_column(membership, path_column);

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, {path_column}, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                   FROM {table}
                  WHERE ({sharing})
                    AND status IN ('active', 'permanent')
                    AND (expires_at IS NULL OR expires_at > ?)
                  ORDER BY COALESCE(importance, 50) DESC, id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_binds = binds.clone();
            all_binds.push(rusqlite::types::Value::from(now_ms));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(all_binds.iter()), |r| {
                    Ok(StoredMemory {
                        id: r.get(0)?,
                        project_path: r.get(1)?,
                        category: r.get(2)?,
                        content: r.get(3)?,
                        importance: r.get(4)?,
                        status: r.get(5)?,
                        expires_at: r.get(6)?,
                        superseded_by_memory_id: r.get(7)?,
                        updated_at: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Load rendered memory rows and both source watermarks from one SQLite read snapshot.
    pub fn load_memory_render_snapshot(
        &self,
        project_path: &str,
        membership: Option<&WorkspaceMembership>,
        now_ms: i64,
    ) -> Result<MemoryRenderSnapshot, McStoreError> {
        let project_paths = membership
            .map(|value| value.union_identities.clone())
            .unwrap_or_else(|| vec![project_path.to_string()]);
        let shadow = is_shadow_project_path(project_path);
        let table = if shadow {
            "shadow_memories"
        } else {
            "mc_memories"
        };
        let path_column = if shadow {
            "shadow_project_path"
        } else {
            "project_path"
        };
        let mutation_table = if shadow {
            "shadow_memory_mutation_log"
        } else {
            "mc_memory_mutation_log"
        };
        let snapshot = self.inner.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let (visibility, mut binds) = if let Some(membership) = membership {
                workspace_union_memory_visibility_filter_for_column(membership, path_column)
            } else {
                (format!("{path_column} = ?"), vec![rusqlite::types::Value::from(project_path.to_string())])
            };
            let sql = format!(
                "SELECT id, {path_column}, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                   FROM {table}
                  WHERE ({visibility})
                    AND status IN ('active', 'permanent')
                    AND (expires_at IS NULL OR expires_at > ?)
                  ORDER BY COALESCE(importance, 50) DESC, id ASC"
            );
            binds.push(rusqlite::types::Value::from(now_ms));
            let mut stmt = tx.prepare(&sql)?;
            let memories = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                    Ok(StoredMemory {
                        id: row.get(0)?,
                        project_path: row.get(1)?,
                        category: row.get(2)?,
                        content: row.get(3)?,
                        importance: row.get(4)?,
                        status: row.get(5)?,
                        expires_at: row.get(6)?,
                        superseded_by_memory_id: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            let placeholders = std::iter::repeat_n("?", project_paths.len())
                .collect::<Vec<_>>()
                .join(", ");
            let max_memory_id = if project_paths.is_empty() {
                0
            } else {
                tx.query_row(
                    &format!(
                        "SELECT COALESCE(MAX(id), 0) FROM {table} WHERE {path_column} IN ({placeholders})"
                    ),
                    rusqlite::params_from_iter(project_paths.iter()),
                    |row| row.get(0),
                )?
            };
            let mutation_cursor = if project_paths.is_empty() {
                0
            } else {
                tx.query_row(
                    &format!(
                        "SELECT COALESCE(MAX(id), 0) FROM {mutation_table} WHERE {path_column} IN ({placeholders})"
                    ),
                    rusqlite::params_from_iter(project_paths.iter()),
                    |row| row.get(0),
                )?
            };
            tx.commit()?;
            Ok(MemoryRenderSnapshot {
                memories,
                revision: MemoryRevision {
                    project_paths: project_paths.clone(),
                    max_memory_id,
                    mutation_cursor,
                },
            })
        })?;
        Ok(snapshot)
    }

    /// Search active/permanent memory content visible to `project_path` with a literal,
    /// case-insensitive SQL LIKE. Workspace visibility is built by the same helper used by
    /// [`Self::load_workspace_union_memories`], keeping search and render on one boundary.
    pub fn search_visible_memory_contents(
        &self,
        project_path: &str,
        query: &str,
    ) -> Result<Vec<StoredMemorySearchRow>, McStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let membership = self.resolve_workspace_membership(project_path)?;
        let (sharing, binds) = match &membership {
            Some(m) => workspace_union_memory_visibility_filter(m),
            None => project_memory_visibility_filter(project_path),
        };
        let pattern = sql_like_pattern(query);

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, project_path, category, content, updated_at
                   FROM mc_memories
                  WHERE ({sharing})
                    AND status IN ('active', 'permanent')
                    AND (expires_at IS NULL OR expires_at > CAST(strftime('%s', 'now') AS INTEGER) * 1000)
                    AND LOWER(content) LIKE ? ESCAPE '\\'
                  ORDER BY updated_at DESC, id ASC
                  LIMIT 100"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_binds = binds.clone();
            all_binds.push(rusqlite::types::Value::from(pattern));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(all_binds.iter()), |r| {
                    Ok(StoredMemorySearchRow {
                        id: r.get(0)?,
                        project_path: r.get(1)?,
                        category: r.get(2)?,
                        content: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Search a session's compartment title and tier text with a literal, case-insensitive
    /// SQL LIKE. The caller supplies the already-resolved session id; no routing is done in
    /// this store layer.
    pub fn search_compartments_like(
        &self,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<StoredCompartmentSearchRow>, McStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = sql_like_pattern(query);
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence, title, content, p1, p2, p3, p4, created_at
                   FROM mc_compartments
                  WHERE session_id = ?1
                    AND (LOWER(title) LIKE ?2 ESCAPE '\\'
                      OR LOWER(content) LIKE ?2 ESCAPE '\\'
                      OR LOWER(COALESCE(p1, '')) LIKE ?2 ESCAPE '\\'
                      OR LOWER(COALESCE(p2, '')) LIKE ?2 ESCAPE '\\'
                      OR LOWER(COALESCE(p3, '')) LIKE ?2 ESCAPE '\\'
                      OR LOWER(COALESCE(p4, '')) LIKE ?2 ESCAPE '\\')
                  ORDER BY sequence DESC
                  LIMIT 100",
            )?;
            let mapped = stmt
                .query_map(params![session_id, pattern], |r| {
                    Ok(StoredCompartmentSearchRow {
                        sequence: r.get(0)?,
                        title: r.get(1)?,
                        content: r.get(2)?,
                        p1: r.get(3)?,
                        p2: r.get(4)?,
                        p3: r.get(5)?,
                        p4: r.get(6)?,
                        created_at: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    pub fn load_chunk_transcripts_for_range(
        &self,
        session_id: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<StoredChunkTranscript>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT compartment_seq, start_ordinal, end_ordinal, transcript_deflate, created_at_ms
                   FROM mc_chunk_transcripts
                  WHERE session_id = ?1
                    AND end_ordinal >= ?2
                    AND start_ordinal <= ?3
                  ORDER BY compartment_seq ASC",
            )?;
            let mapped = stmt
                .query_map(params![session_id, start, end], |r| {
                    let blob: Vec<u8> = r.get(3)?;
                    Ok(StoredChunkTranscript {
                        compartment_seq: r.get(0)?,
                        start_ordinal: r.get(1)?,
                        end_ordinal: r.get(2)?,
                        transcript: decompress_transcript(&blob).ok(),
                        created_at_ms: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    pub fn load_chunk_transcript_for_message(
        &self,
        session_id: &str,
        ordinal: i64,
    ) -> Result<Option<StoredChunkTranscript>, McStoreError> {
        Ok(self
            .load_chunk_transcripts_for_range(session_id, ordinal, ordinal)?
            .into_iter()
            .next())
    }

    pub fn insert_note(&self, input: NoteInput<'_>) -> Result<StoredNote, McStoreError> {
        let content = input.content.trim();
        self.inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_notes
                   (project_path, session_id, content, status, surface_condition, anchor_block_id,
                    created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?6)",
                    params![
                        input.project_path,
                        input.session_id,
                        content,
                        input
                            .surface_condition
                            .map(str::trim)
                            .filter(|s| !s.is_empty()),
                        input.anchor_block_id,
                        input.now_ms,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                load_note_tx(tx, id)
            })
            .map_err(Into::into)
    }

    pub fn read_notes(
        &self,
        project_path: &str,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredNote>, McStoreError> {
        let limit = limit.clamp(1, 100) as i64;
        let offset = i64::try_from(offset)
            .map_err(|_| McStoreError::Serde("note offset exceeds i64".to_string()))?;
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_path, session_id, content, status, surface_condition,
                        anchor_block_id, created_at_ms, updated_at_ms
                   FROM mc_notes
                  WHERE project_path = ?1 AND session_id = ?2 AND status = 'active'
                  ORDER BY updated_at_ms DESC, id DESC
                  LIMIT ?3 OFFSET ?4",
            )?;
            let mapped = stmt
                .query_map(
                    params![project_path, session_id, limit, offset],
                    stored_note_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    pub fn update_note_content(
        &self,
        project_path: &str,
        session_id: &str,
        note_id: i64,
        content: &str,
        now_ms: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        let content = content.trim();
        self.inner
            .with_conn_fenced(|tx| {
                let changed = tx.execute(
                    "UPDATE mc_notes
                    SET content = ?4, updated_at_ms = ?5
                  WHERE id = ?1 AND project_path = ?2 AND session_id = ?3 AND status = 'active'",
                    params![note_id, project_path, session_id, content, now_ms],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                load_note_tx(tx, note_id).map(Some)
            })
            .map_err(Into::into)
    }

    pub fn dismiss_note(
        &self,
        project_path: &str,
        session_id: &str,
        note_id: i64,
        resolution: Option<&str>,
        now_ms: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        self.inner
            .with_conn_fenced(|tx| {
                let Some(mut note) = load_note_scoped_tx(tx, project_path, session_id, note_id)?
                else {
                    return Ok(None);
                };
                if note.status != "active" {
                    return Ok(None);
                }
                if let Some(resolution) = resolution.map(str::trim).filter(|s| !s.is_empty()) {
                    note.content =
                        format!("{}\n\nDismissal resolution: {resolution}", note.content);
                }
                tx.execute(
                    "UPDATE mc_notes
                    SET content = ?2, status = 'dismissed', updated_at_ms = ?3
                  WHERE id = ?1",
                    params![note_id, note.content, now_ms],
                )?;
                load_note_tx(tx, note_id).map(Some)
            })
            .map_err(Into::into)
    }

    pub fn search_notes_like(
        &self,
        project_path: &str,
        session_id: &str,
        query: &str,
    ) -> Result<Vec<StoredNoteSearchRow>, McStoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = sql_like_pattern(query);
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, content, status, surface_condition, updated_at_ms
                   FROM mc_notes
                  WHERE project_path = ?1 AND session_id = ?2
                    AND (LOWER(content) LIKE ?3 ESCAPE '\\'
                      OR LOWER(COALESCE(surface_condition, '')) LIKE ?3 ESCAPE '\\')
                  ORDER BY updated_at_ms DESC, id DESC
                  LIMIT 100",
            )?;
            let mapped = stmt
                .query_map(params![project_path, session_id, pattern], |r| {
                    Ok(StoredNoteSearchRow {
                        id: r.get(0)?,
                        content: r.get(1)?,
                        status: r.get(2)?,
                        surface_condition: r.get(3)?,
                        updated_at_ms: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Load active user-memory contents (the `<user-profile>` baseline source), ordered
    /// `promoted_at ASC, id ASC`. The id tiebreaker is load-bearing: `promoted_at` can
    /// tie at ms granularity and a non-deterministic order would drift the rendered
    /// bytes between passes. Returns just the contents (the render is `- <content>`).
    pub fn load_active_user_memories(&self) -> Result<Vec<String>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content FROM mc_user_memories WHERE status = 'active'
                 ORDER BY promoted_at ASC, id ASC",
            )?;
            let mapped = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// The highest memory-mutation-log id across the given project identities (the union,
    /// or a single-element slice). The cursor a baseline re-render (HARD) folds the
    /// corrections up to, and the watermark a delta pass (SOFT) reads new corrections
    /// past. 0 when the log is empty. Union-scoped to match
    /// [`Self::memory_mutations_for_render`].
    pub fn max_memory_mutation_id(&self, project_paths: &[String]) -> Result<i64, McStoreError> {
        if project_paths.is_empty() {
            return Ok(0);
        }
        if project_paths
            .iter()
            .any(|path| is_shadow_project_path(path))
        {
            let mut projects: Vec<String> = project_paths
                .iter()
                .filter(|path| is_shadow_project_path(path))
                .cloned()
                .collect();
            projects.sort_unstable();
            projects.dedup();
            if projects.is_empty() {
                return Ok(0);
            }
            let ph = std::iter::repeat_n("?", projects.len())
                .collect::<Vec<_>>()
                .join(", ");
            let max = self.inner.with_conn(|conn| {
                let sql = format!(
                    "SELECT COALESCE(MAX(id), 0) FROM shadow_memory_mutation_log
                     WHERE shadow_project_path IN ({ph})"
                );
                let v: i64 =
                    conn.query_row(&sql, rusqlite::params_from_iter(projects.iter()), |r| {
                        r.get(0)
                    })?;
                Ok(v)
            })?;
            return Ok(max);
        }
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");
        let max = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT COALESCE(MAX(id), 0) FROM mc_memory_mutation_log
                 WHERE project_path IN ({ph})"
            );
            let v: i64 =
                conn.query_row(&sql, rusqlite::params_from_iter(projects.iter()), |r| {
                    r.get(0)
                })?;
            Ok(v)
        })?;
        Ok(max)
    }

    /// The highest memory id across the given project identities (the union, or a
    /// single-element slice). A baseline re-render (HARD) folds memories up to this; a
    /// delta pass (SOFT) renders memories with `id > max_memory_id` as `<new-memories>`.
    /// 0 when there are no memories.
    pub fn max_memory_id(&self, project_paths: &[String]) -> Result<i64, McStoreError> {
        if project_paths.is_empty() {
            return Ok(0);
        }
        if project_paths
            .iter()
            .any(|path| is_shadow_project_path(path))
        {
            let mut projects: Vec<String> = project_paths
                .iter()
                .filter(|path| is_shadow_project_path(path))
                .cloned()
                .collect();
            projects.sort_unstable();
            projects.dedup();
            if projects.is_empty() {
                return Ok(0);
            }
            let ph = std::iter::repeat_n("?", projects.len())
                .collect::<Vec<_>>()
                .join(", ");
            let max = self.inner.with_conn(|conn| {
                let sql = format!(
                    "SELECT COALESCE(MAX(id), 0) FROM shadow_memories WHERE shadow_project_path IN ({ph})"
                );
                let v: i64 = conn.query_row(
                    &sql,
                    rusqlite::params_from_iter(projects.iter()),
                    |r| r.get(0),
                )?;
                Ok(v)
            })?;
            return Ok(max);
        }
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");
        let max = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT COALESCE(MAX(id), 0) FROM mc_memories WHERE project_path IN ({ph})"
            );
            let v: i64 =
                conn.query_row(&sql, rusqlite::params_from_iter(projects.iter()), |r| {
                    r.get(0)
                })?;
            Ok(v)
        })?;
        Ok(max)
    }
}

/// Test-support seed helpers for sibling crates and this crate's own tests (gated
/// behind `test-support` or `cfg(test)` so the writers never ship in production).
/// mc-module composes over this store and needs to populate memories/mutations in
/// its tests.
#[cfg(any(test, feature = "test-support"))]
impl McStore {
    /// Insert an active memory for `project_path`.
    pub fn seed_memory(
        &self,
        id: i64,
        project_path: &str,
        category: &str,
        content: &str,
        importance: i64,
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO mc_memories (id, project_path, category, content, normalized_hash,
                                          importance, status, first_seen_at, created_at,
                                          updated_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 0, 0, 0, 0)",
                params![
                    id,
                    project_path,
                    category,
                    content,
                    format!("h{id}"),
                    importance
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Insert an active memory with an explicit `expires_at` (ms) for `project_path` —
    /// used to test the frozen expiry cutoff (a memory live under one cutoff, expired
    /// under a later one).
    pub fn seed_expiring_memory(
        &self,
        id: i64,
        project_path: &str,
        category: &str,
        content: &str,
        importance: i64,
        expires_at: i64,
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO mc_memories (id, project_path, category, content, normalized_hash,
                                          importance, status, expires_at, first_seen_at,
                                          created_at, updated_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, 0, 0, 0, 0)",
                params![
                    id,
                    project_path,
                    category,
                    content,
                    format!("h{id}"),
                    importance,
                    expires_at
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Add `project_path` to a workspace named `workspace` (creating it with the given
    /// sorted share categories), so `workspace_fingerprint` reflects the membership.
    pub fn seed_workspace_member(
        &self,
        workspace: &str,
        project_path: &str,
        share_categories_json: &str,
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO mc_workspaces (name, share_categories) VALUES (?1, ?2)
                 ON CONFLICT(name) DO NOTHING",
                params![workspace, share_categories_json],
            )?;
            let ws_id: i64 = tx.query_row(
                "SELECT id FROM mc_workspaces WHERE name = ?1",
                params![workspace],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at)
                 VALUES (?1, ?2, ?2, ?2, 0)",
                params![ws_id, project_path],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Append a memory-mutation-log row for `project_path`.
    pub fn seed_mutation(
        &self,
        project_path: &str,
        mutation_type: &str,
        target_memory_id: i64,
        new_content: &str,
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO mc_memory_mutation_log
                    (project_path, mutation_type, target_memory_id, new_content, queued_at)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                params![project_path, mutation_type, target_memory_id, new_content],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

fn upsert_compartment_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    c: &StoredCompartment,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO mc_compartments
           (session_id, sequence, start_message, end_message, start_message_id,
            end_message_id, start_date, end_date, title, content, p1, p2, p3, p4,
            importance, episode_type, legacy, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT(session_id, sequence) DO UPDATE SET
            start_message = excluded.start_message,
            end_message = excluded.end_message,
            start_message_id = excluded.start_message_id,
            end_message_id = excluded.end_message_id,
            start_date = excluded.start_date,
            end_date = excluded.end_date,
            title = excluded.title,
            content = excluded.content,
            p1 = excluded.p1,
            p2 = excluded.p2,
            p3 = excluded.p3,
            p4 = excluded.p4,
            importance = excluded.importance,
            episode_type = excluded.episode_type,
            legacy = excluded.legacy,
            created_at = excluded.created_at",
        params![
            session_id,
            c.sequence,
            c.start_message,
            c.end_message,
            &c.start_message_id,
            &c.end_message_id,
            c.start_date.as_deref(),
            c.end_date.as_deref(),
            &c.title,
            &c.content,
            c.p1.as_deref(),
            c.p2.as_deref(),
            c.p3.as_deref(),
            c.p4.as_deref(),
            c.importance as i64,
            c.episode_type.as_deref(),
            c.legacy as i64,
            c.created_at,
        ],
    )?;
    Ok(())
}

fn replace_shadow_workspace_tx(
    tx: &rusqlite::Transaction<'_>,
    shadow_project_path: &str,
    workspace: Option<&ShadowWorkspaceRow>,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM mc_workspaces
          WHERE id IN (
              SELECT workspace_id FROM mc_workspace_members WHERE project_path = ?1
          )",
        params![shadow_project_path],
    )?;
    let Some(workspace) = workspace else {
        return Ok(());
    };
    let share_categories = serde_json::to_string(&workspace.share_categories)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.execute(
        "INSERT INTO mc_workspaces (name, created_at, updated_at, share_categories)
         VALUES (?1, 0, 0, ?2)",
        params![&workspace.name, share_categories],
    )?;
    let workspace_id = tx.last_insert_rowid();
    for member in &workspace.members {
        tx.execute(
            "INSERT INTO mc_workspace_members
                (workspace_id, project_path, display_name, display_path, added_at)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                workspace_id,
                &member.project_path,
                &member.display_name,
                &member.display_path
            ],
        )?;
    }
    Ok(())
}

fn replace_shadow_memories_tx(
    tx: &rusqlite::Transaction<'_>,
    memories: &[ShadowMemoryRow],
) -> rusqlite::Result<()> {
    if memories.is_empty() {
        return Ok(());
    }
    for memory in memories {
        tx.execute(
            "INSERT INTO shadow_memories
               (shadow_project_path, id, category, content, normalized_hash, importance,
                scope, shareable, source_session_id, source_type, seen_count, retrieval_count,
                first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
                status, expires_at, verification_status, verified_at, classified_at,
                superseded_by_memory_id, merged_from, metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)
             ON CONFLICT(shadow_project_path, id) DO UPDATE SET
                category = excluded.category,
                content = excluded.content,
                normalized_hash = excluded.normalized_hash,
                importance = excluded.importance,
                scope = excluded.scope,
                shareable = excluded.shareable,
                source_session_id = excluded.source_session_id,
                source_type = excluded.source_type,
                seen_count = excluded.seen_count,
                retrieval_count = excluded.retrieval_count,
                first_seen_at = excluded.first_seen_at,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                last_seen_at = excluded.last_seen_at,
                last_retrieved_at = excluded.last_retrieved_at,
                status = excluded.status,
                expires_at = excluded.expires_at,
                verification_status = excluded.verification_status,
                verified_at = excluded.verified_at,
                classified_at = excluded.classified_at,
                superseded_by_memory_id = excluded.superseded_by_memory_id,
                merged_from = excluded.merged_from,
                metadata_json = excluded.metadata_json",
            params![
                &memory.project_path,
                memory.id,
                &memory.category,
                &memory.content,
                &memory.normalized_hash,
                memory.importance.map(i64::from),
                &memory.scope,
                memory.shareable as i64,
                memory.source_session_id.as_deref(),
                memory.source_type.as_deref(),
                memory.seen_count,
                memory.retrieval_count,
                memory.first_seen_at,
                memory.created_at,
                memory.updated_at,
                memory.last_seen_at,
                memory.last_retrieved_at,
                &memory.status,
                memory.expires_at,
                &memory.verification_status,
                memory.verified_at,
                memory.classified_at,
                memory.superseded_by_memory_id,
                memory.merged_from.as_deref(),
                memory.metadata_json.as_deref(),
            ],
        )?;
    }
    Ok(())
}

fn replace_shadow_memory_mutations_tx(
    tx: &rusqlite::Transaction<'_>,
    mutations: &[ShadowMemoryMutationRow],
) -> rusqlite::Result<()> {
    if mutations.is_empty() {
        return Ok(());
    }
    for row in mutations {
        let mutation = &row.mutation;
        tx.execute(
            "INSERT INTO shadow_memory_mutation_log
               (shadow_project_path, id, mutation_type, target_memory_id, superseded_by_id,
                category, new_content, queued_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(shadow_project_path, id) DO UPDATE SET
                mutation_type = excluded.mutation_type,
                target_memory_id = excluded.target_memory_id,
                superseded_by_id = excluded.superseded_by_id,
                category = excluded.category,
                new_content = excluded.new_content,
                queued_at = excluded.queued_at",
            params![
                &row.project_path,
                mutation.id,
                &mutation.mutation_type,
                mutation.target_memory_id,
                mutation.superseded_by_id,
                mutation.category.as_deref(),
                mutation.new_content.as_deref(),
                mutation.queued_at,
            ],
        )?;
    }
    Ok(())
}

fn insert_compartment_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    sequence: i64,
    c: &StoredCompartment,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO mc_compartments
           (session_id, sequence, start_message, end_message, start_message_id,
            end_message_id, start_date, end_date, title, content, p1, p2, p3, p4,
            importance, episode_type, legacy, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![
            session_id,
            sequence,
            c.start_message,
            c.end_message,
            &c.start_message_id,
            &c.end_message_id,
            c.start_date.as_deref(),
            c.end_date.as_deref(),
            &c.title,
            &c.content,
            c.p1.as_deref(),
            c.p2.as_deref(),
            c.p3.as_deref(),
            c.p4.as_deref(),
            c.importance as i64,
            c.episode_type.as_deref(),
            c.legacy as i64,
            c.created_at,
        ],
    )?;
    Ok(())
}

fn append_compartments_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    compartments: &[StoredCompartment],
) -> rusqlite::Result<()> {
    if compartments.is_empty() {
        return Ok(());
    }
    let tail = next_compartment_sequence_tx(tx, session_id)? - 1;
    for (idx, compartment) in compartments.iter().enumerate() {
        insert_compartment_tx(tx, session_id, tail + idx as i64 + 1, compartment)?;
    }
    Ok(())
}

fn next_compartment_sequence_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM mc_compartments WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )
}

fn insert_chunk_transcripts_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    first_sequence: i64,
    compartments: &[StoredCompartment],
    transcript: &str,
) -> rusqlite::Result<()> {
    if compartments.is_empty() {
        return Ok(());
    }
    let compressed = match compress_transcript(transcript) {
        Ok(compressed) if compressed.len() <= MAX_CHUNK_TRANSCRIPT_COMPRESSED_BYTES => compressed,
        _ => return Ok(()),
    };
    for (idx, compartment) in compartments.iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO mc_chunk_transcripts
               (session_id, compartment_seq, start_ordinal, end_ordinal, transcript_deflate, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                first_sequence + idx as i64,
                compartment.start_message,
                compartment.end_message,
                &compressed,
                compartment.created_at,
            ],
        )?;
    }
    evict_chunk_transcripts_tx(tx, session_id)
}

fn evict_chunk_transcripts_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> rusqlite::Result<()> {
    loop {
        let total: i64 = tx.query_row(
            "SELECT COALESCE(SUM(LENGTH(transcript_deflate)), 0)
               FROM mc_chunk_transcripts WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        if total <= MAX_SESSION_TRANSCRIPT_COMPRESSED_BYTES {
            return Ok(());
        }
        let victim: Option<i64> = tx
            .query_row(
                "SELECT compartment_seq
                   FROM mc_chunk_transcripts
                  WHERE session_id = ?1
                  ORDER BY created_at_ms ASC, compartment_seq ASC
                  LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(victim) = victim else {
            return Ok(());
        };
        tx.execute(
            "DELETE FROM mc_chunk_transcripts WHERE session_id = ?1 AND compartment_seq = ?2",
            params![session_id, victim],
        )?;
    }
}

fn compress_transcript(transcript: &str) -> std::io::Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(transcript.as_bytes())?;
    encoder.finish()
}

fn decompress_transcript(blob: &[u8]) -> std::io::Result<String> {
    let mut decoder = DeflateDecoder::new(blob);
    let mut out = String::new();
    decoder.read_to_string(&mut out)?;
    Ok(out)
}

fn tag_row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<McTagRow> {
    Ok(McTagRow {
        tag_number: r.get(0)?,
        block_id: r.get(1)?,
        kind: r.get(2)?,
        token_count: r.get(3)?,
        created_at_ms: r.get(4)?,
        source_bytes: r.get(5)?,
    })
}

fn stored_note_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNote> {
    Ok(StoredNote {
        id: r.get(0)?,
        project_path: r.get(1)?,
        session_id: r.get(2)?,
        content: r.get(3)?,
        status: r.get(4)?,
        surface_condition: r.get(5)?,
        anchor_block_id: r.get(6)?,
        created_at_ms: r.get(7)?,
        updated_at_ms: r.get(8)?,
    })
}

fn load_note_tx(tx: &rusqlite::Transaction<'_>, id: i64) -> rusqlite::Result<StoredNote> {
    tx.query_row(
        "SELECT id, project_path, session_id, content, status, surface_condition,
                anchor_block_id, created_at_ms, updated_at_ms
           FROM mc_notes WHERE id = ?1",
        params![id],
        stored_note_from_row,
    )
}

fn load_note_scoped_tx(
    tx: &rusqlite::Transaction<'_>,
    project_path: &str,
    session_id: &str,
    id: i64,
) -> rusqlite::Result<Option<StoredNote>> {
    tx.query_row(
        "SELECT id, project_path, session_id, content, status, surface_condition,
                anchor_block_id, created_at_ms, updated_at_ms
           FROM mc_notes
          WHERE id = ?1 AND project_path = ?2 AND session_id = ?3",
        params![id, project_path, session_id],
        stored_note_from_row,
    )
    .optional()
}

fn promote_facts_tx(
    tx: &rusqlite::Transaction<'_>,
    project_path: &str,
    facts: &[FactCandidate],
) -> rusqlite::Result<Vec<PromotedRef>> {
    let mut active_content = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT content FROM mc_memories
             WHERE project_path = ?1 AND status IN ('active', 'permanent')",
        )?;
        let rows = stmt.query_map(params![project_path], |r| r.get::<_, String>(0))?;
        for row in rows {
            active_content.insert(row?);
        }
    }

    let mut next_nonce: i64 = tx.query_row(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM mc_memories",
        [],
        |r| r.get(0),
    )?;
    let mut promoted = Vec::new();

    for fact in facts {
        if fact.category.trim().is_empty() || fact.content.trim().is_empty() {
            continue;
        }
        if active_content.contains(&fact.content) {
            continue;
        }

        let normalized_hash = format!(
            "historian-exact:{:016x}:{next_nonce}",
            stable_content_hash(&fact.content)
        );
        tx.execute(
            "INSERT INTO mc_memories
               (project_path, category, content, normalized_hash, importance,
                source_session_id, source_type, seen_count, retrieval_count,
                first_seen_at, created_at, updated_at, last_seen_at, status,
                expires_at, verification_status)
             VALUES (?1,?2,?3,?4,?5,?6,'historian',1,0,0,0,0,0,'active',?7,'unverified')",
            params![
                project_path,
                &fact.category,
                &fact.content,
                normalized_hash,
                fact.importance.map(i64::from),
                fact.source_session_id.as_deref(),
                fact.expires_at,
            ],
        )?;
        let memory_id = tx.last_insert_rowid();
        active_content.insert(fact.content.clone());
        promoted.push(PromotedRef {
            memory_id,
            content: fact.content.clone(),
        });
        next_nonce += 1;
    }

    Ok(promoted)
}

const MEMORY_FULL_SELECT_BY_ID: &str =
    "SELECT id, project_path, category, content, normalized_hash, importance, scope,
            shareable, source_session_id, source_type, seen_count, retrieval_count,
            first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
            status, expires_at, verification_status, verified_at, classified_at,
            superseded_by_memory_id, merged_from, metadata_json
       FROM mc_memories WHERE id = ?1";

fn stored_memory_full_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMemoryFull> {
    Ok(StoredMemoryFull {
        id: r.get(0)?,
        project_path: r.get(1)?,
        category: r.get(2)?,
        content: r.get(3)?,
        normalized_hash: r.get(4)?,
        importance: r.get::<_, Option<i64>>(5)?.map(|v| v as i32),
        scope: r.get(6)?,
        shareable: r.get::<_, i64>(7)? as i32,
        source_session_id: r.get(8)?,
        source_type: r.get(9)?,
        seen_count: r.get::<_, Option<i64>>(10)?.unwrap_or(0),
        retrieval_count: r.get::<_, Option<i64>>(11)?.unwrap_or(0),
        first_seen_at: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
        last_seen_at: r.get(15)?,
        last_retrieved_at: r.get(16)?,
        status: r
            .get::<_, Option<String>>(17)?
            .unwrap_or_else(|| "active".to_string()),
        expires_at: r.get(18)?,
        verification_status: r
            .get::<_, Option<String>>(19)?
            .unwrap_or_else(|| "unverified".to_string()),
        verified_at: r.get(20)?,
        classified_at: r.get(21)?,
        superseded_by_memory_id: r.get(22)?,
        merged_from: r.get(23)?,
        metadata_json: r.get(24)?,
    })
}

fn load_memory_full_tx(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
) -> rusqlite::Result<Option<StoredMemoryFull>> {
    tx.query_row(
        MEMORY_FULL_SELECT_BY_ID,
        params![id],
        stored_memory_full_from_row,
    )
    .optional()
}

struct MemoryMutationAppend<'a> {
    project_path: &'a str,
    mutation_type: &'a str,
    target_memory_id: i64,
    superseded_by_id: Option<i64>,
    category: Option<&'a str>,
    new_content: Option<&'a str>,
    queued_at: i64,
}

fn append_memory_mutation_tx(
    tx: &rusqlite::Transaction<'_>,
    mutation: MemoryMutationAppend<'_>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO mc_memory_mutation_log
            (project_path, mutation_type, target_memory_id, superseded_by_id,
             category, new_content, queued_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            mutation.project_path,
            mutation.mutation_type,
            mutation.target_memory_id,
            mutation.superseded_by_id,
            mutation.category,
            mutation.new_content,
            mutation.queued_at,
        ],
    )?;
    Ok(())
}

fn merge_archive_reason(existing: Option<&str>, reason: &str) -> String {
    let mut object = existing
        .and_then(|json| serde_json::from_str::<Value>(json).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "archive_reason".to_string(),
        Value::String(reason.to_string()),
    );
    Value::Object(object).to_string()
}

fn merged_from_json(rows: &[StoredMemoryFull]) -> String {
    let mut ids = BTreeSet::new();
    for row in rows {
        ids.insert(row.id);
        if let Some(raw) = &row.merged_from {
            if let Ok(Value::Array(values)) = serde_json::from_str::<Value>(raw) {
                for value in values {
                    if let Some(id) = value.as_i64() {
                        ids.insert(id);
                    }
                }
            }
        }
    }
    let ids: Vec<i64> = ids.into_iter().collect();
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string())
}

fn project_memory_visibility_filter(project_path: &str) -> (String, Vec<rusqlite::types::Value>) {
    (
        "project_path = ?".to_string(),
        vec![rusqlite::types::Value::from(project_path.to_string())],
    )
}

fn workspace_union_memory_visibility_filter(
    membership: &WorkspaceMembership,
) -> (String, Vec<rusqlite::types::Value>) {
    workspace_union_memory_visibility_filter_for_column(membership, "project_path")
}

fn workspace_union_memory_visibility_filter_for_column(
    membership: &WorkspaceMembership,
    path_column: &str,
) -> (String, Vec<rusqlite::types::Value>) {
    let WorkspaceMembership {
        union_identities,
        own_identity,
        share_categories,
        ..
    } = membership;

    let foreign: Vec<&String> = union_identities
        .iter()
        .filter(|p| *p != own_identity)
        .collect();

    let mut binds: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::from(own_identity.clone())];
    let mut sharing = format!("{path_column} = ?");
    if !foreign.is_empty() && !share_categories.is_empty() {
        let fph = std::iter::repeat_n("?", foreign.len())
            .collect::<Vec<_>>()
            .join(", ");
        let cph = std::iter::repeat_n("?", share_categories.len())
            .collect::<Vec<_>>()
            .join(", ");
        sharing.push_str(&format!(
            " OR ({path_column} IN ({fph}) AND category IN ({cph}))"
        ));
        for p in &foreign {
            binds.push(rusqlite::types::Value::from((*p).clone()));
        }
        for c in share_categories {
            binds.push(rusqlite::types::Value::from(c.clone()));
        }
    }

    (sharing, binds)
}

fn sql_like_pattern(query: &str) -> String {
    let mut escaped = String::new();
    for ch in query.trim().to_lowercase().chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    format!("%{escaped}%")
}

/// Compute the ctx_memory normalized hash used for duplicate detection. This mirrors the
/// plugin path: lowercase, collapse whitespace runs to one space, trim, then MD5 hex.
pub fn compute_normalized_memory_hash(content: &str) -> String {
    let normalized = content
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let digest = md5::compute(normalized.as_bytes());
    format!("{digest:032x}")
}

fn stable_content_hash(content: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn is_shadow_project_path(project_path: &str) -> bool {
    project_path.starts_with(SHADOW_SESSION_PREFIX)
}

fn idle_historian_after_success(firing_seq: u64) -> HistorianDurableState {
    HistorianDurableState {
        firing_seq,
        ..HistorianDurableState::default()
    }
}

/// Coalesce mutation-log rows to one per target memory: deterministic latest-wins with
/// TERMINAL precedence (terminal always outranks a non-terminal `update`, regardless of
/// id order). Among rows of the same terminality, the later id wins. Sorted by id for a
/// stable render order.
fn coalesce_mutations(rows: Vec<StoredMemoryMutation>) -> Vec<StoredMemoryMutation> {
    use std::collections::HashMap;
    let mut chosen: HashMap<i64, StoredMemoryMutation> = HashMap::new();
    // rows arrive id-ASC; iterate in that order so "later id wins" = last-write-wins
    // for same-terminality, and the terminal-precedence guard handles the rest.
    for candidate in rows {
        match chosen.get(&candidate.target_memory_id) {
            None => {
                chosen.insert(candidate.target_memory_id, candidate);
            }
            Some(current) => {
                let current_terminal = current.is_terminal();
                let candidate_terminal = candidate.is_terminal();
                if current_terminal && !candidate_terminal {
                    // keep the terminal; a later update can't resurrect it.
                    continue;
                }
                // candidate-terminal-over-current-nonterminal, OR same-terminality
                // later-id → candidate wins.
                chosen.insert(candidate.target_memory_id, candidate);
            }
        }
    }
    let mut out: Vec<StoredMemoryMutation> = chosen.into_values().collect();
    out.sort_by_key(|m| m.id);
    out
}

fn capped_trace_error(error: &str) -> String {
    error.chars().take(2000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend};

    fn descriptor(dir: &std::path::Path) -> StorageDescriptor {
        StorageDescriptor {
            module_id: "magic-context-test".to_string(),
            storage_namespace: "mc_cache".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.join("store.db").to_string_lossy().to_string(),
            },
        }
    }

    fn command_ledger_ids(store: &McStore, session_id: &str) -> Vec<String> {
        store
            .inner
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT command_id
                     FROM mc_reduce_command_ledger
                     WHERE session_id = ?1
                     ORDER BY queued_at_ms ASC, command_id ASC",
                )?;
                let rows = statement.query_map(params![session_id], |row| row.get(0))?;
                let mut command_ids = Vec::new();
                for row in rows {
                    command_ids.push(row?);
                }
                Ok(command_ids)
            })
            .unwrap()
    }

    fn insert_input<'a>(
        project_path: &'a str,
        category: &'a str,
        content: &'a str,
        now_ms: i64,
    ) -> InsertMemoryInput<'a> {
        InsertMemoryInput {
            project_path,
            category,
            content,
            source_session_id: None,
            source_type: Some("tool"),
            importance: Some(50),
            expires_at: None,
            metadata_json: None,
            now_ms,
        }
    }

    #[test]
    fn bootstrap_load_returns_uninitialized_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let loaded = store.load("ses_a").unwrap();
        assert!(!loaded.meta.initialized);
        assert_eq!(loaded.row_version, None);
        assert_eq!(loaded.core, CoreState::default());
    }

    #[test]
    fn commit_then_load_roundtrips_and_bumps_row_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();

        let core = CoreState {
            boundary_id: "b1".into(),
            ..Default::default()
        };
        let meta = ModuleMeta {
            initialized: true,
            last_render_config: "cfg1".into(),
            coverage_ordinal: Some(42),
            last_todo_state: Some(
                r#"[{"content":"persist me","status":"pending","priority":"high"}]"#.into(),
            ),
            m1_revision: 0,
            ..Default::default()
        };

        let v1 = store.commit("ses_a", None, &core, &meta).unwrap();
        assert_eq!(v1, 1);

        let loaded = store.load("ses_a").unwrap();
        assert_eq!(loaded.row_version, Some(1));
        assert_eq!(loaded.core.boundary_id, "b1");
        assert_eq!(loaded.meta, meta);

        let v2 = store.commit("ses_a", Some(1), &core, &meta).unwrap();
        assert_eq!(v2, 2);
    }

    #[test]
    fn stale_cas_expectation_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let core = CoreState::default();
        let meta = ModuleMeta::default();

        store.commit("ses_a", None, &core, &meta).unwrap(); // row_version now 1
                                                            // A writer that still thinks the row is absent must conflict.
        let err = store.commit("ses_a", None, &core, &meta).unwrap_err();
        match err {
            McStoreError::CasConflict { expected, found } => {
                assert_eq!(expected, None);
                assert_eq!(found, 1);
            }
            other => panic!("expected CasConflict, got {other}"),
        }
    }

    #[test]
    fn cache_commit_rejects_an_advanced_memory_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        store
            .insert_memory(insert_input(project, "CONSTRAINTS", "old", 1))
            .unwrap();
        let snapshot = store.load_memory_render_snapshot(project, None, 2).unwrap();
        store
            .insert_memory(insert_input(project, "CONSTRAINTS", "new", 3))
            .unwrap();

        let error = store
            .commit_with_consumed_drops(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta::default(),
                &[],
                Some(&snapshot.revision),
            )
            .unwrap_err();
        assert!(matches!(error, McStoreError::CasConflict { .. }));
        assert!(store.load("ses").unwrap().row_version.is_none());
    }

    #[test]
    fn pending_agent_drops_delete_only_inside_successful_commit_tx() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        assert_eq!(
            store
                .append_pending_agent_drops("ses", &["a#0".to_string(), "a#0".to_string()], 7)
                .unwrap(),
            1,
            "duplicate pending ids are ignored while still queued"
        );
        let queued = store.load_pending_agent_drops("ses").unwrap();
        assert_eq!(queued.len(), 1);

        let core = CoreState::default();
        let meta = ModuleMeta::default();
        let conflict =
            store.commit_with_consumed_drops("ses", Some(99), &core, &meta, &[queued[0].id], None);
        assert!(matches!(conflict, Err(McStoreError::CasConflict { .. })));
        assert_eq!(
            store.load_pending_agent_drops("ses").unwrap().len(),
            1,
            "a failed fenced commit leaves queued drops for retry"
        );

        store
            .commit_with_consumed_drops("ses", None, &core, &meta, &[queued[0].id], None)
            .unwrap();
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
        assert!(command_ledger_ids(&store, "ses").is_empty());
    }

    #[test]
    fn command_id_duplicate_is_recognized_while_drops_are_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];

        let first = store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 1)
            .unwrap();
        assert_eq!(
            first,
            AppendOutcome {
                queued: 1,
                duplicate: false,
            }
        );
        let pending = store.load_pending_agent_drops("ses").unwrap();

        let retry = store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 2)
            .unwrap();
        assert_eq!(
            retry,
            AppendOutcome {
                queued: 0,
                duplicate: true,
            }
        );
        assert_eq!(store.load_pending_agent_drops("ses").unwrap(), pending);
        assert_eq!(command_ledger_ids(&store, "ses"), vec!["tool-use-1"]);
    }

    #[test]
    fn command_id_duplicate_survives_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];
        store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 1)
            .unwrap();
        let pending = store.load_pending_agent_drops("ses").unwrap();
        store
            .commit_with_consumed_drops(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta::default(),
                &[pending[0].id],
                None,
            )
            .unwrap();
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        let retry = store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 2)
            .unwrap();
        assert_eq!(
            retry,
            AppendOutcome {
                queued: 0,
                duplicate: true,
            }
        );
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[test]
    fn different_command_id_requeues_after_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];
        store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 1)
            .unwrap();
        let pending = store.load_pending_agent_drops("ses").unwrap();
        store
            .commit_with_consumed_drops(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta::default(),
                &[pending[0].id],
                None,
            )
            .unwrap();

        let next = store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-2"), &target_ids, 2)
            .unwrap();
        assert_eq!(
            next,
            AppendOutcome {
                queued: 1,
                duplicate: false,
            }
        );
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);
    }

    #[test]
    fn failed_command_append_rolls_back_ledger_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .inner
            .with_conn(|conn| {
                conn.execute_batch(
                    "CREATE TRIGGER fail_pending_agent_drop
                     BEFORE INSERT ON pending_agent_drops
                     BEGIN
                         SELECT RAISE(ABORT, 'forced pending append failure');
                     END;",
                )?;
                Ok(())
            })
            .unwrap();
        let target_ids = vec!["a#0".to_string()];

        assert!(store
            .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 1)
            .is_err());
        assert!(command_ledger_ids(&store, "ses").is_empty());
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        store
            .inner
            .with_conn(|conn| {
                conn.execute_batch("DROP TRIGGER fail_pending_agent_drop")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .append_pending_agent_drops_with_command("ses", Some("tool-use-1"), &target_ids, 2)
                .unwrap(),
            AppendOutcome {
                queued: 1,
                duplicate: false,
            }
        );
    }

    #[test]
    fn command_id_ledger_retains_rows_past_512_commands() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];

        for queued_at_ms in 0..513i64 {
            let command_id = format!("command-{queued_at_ms:03}");
            let outcome = store
                .append_pending_agent_drops_with_command(
                    "ses",
                    Some(&command_id),
                    &target_ids,
                    queued_at_ms,
                )
                .unwrap();
            assert!(!outcome.duplicate);
        }

        let command_ids = command_ledger_ids(&store, "ses");
        assert_eq!(command_ids.len(), 513);
        assert_eq!(command_ids.first().map(String::as_str), Some("command-000"));
        assert_eq!(command_ids.last().map(String::as_str), Some("command-512"));

        let oldest_retry = store
            .append_pending_agent_drops_with_command("ses", Some("command-000"), &target_ids, 513)
            .unwrap();
        assert!(oldest_retry.duplicate);
    }

    #[test]
    fn reset_shadow_session_clears_command_ledger_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let session_id = "shadow:cleanup";
        let target_ids = vec!["a#0".to_string()];
        store
            .append_pending_agent_drops_with_command(session_id, Some("tool-use-1"), &target_ids, 1)
            .unwrap();
        store.reset_shadow_session(session_id, session_id).unwrap();

        assert_eq!(
            store
                .append_pending_agent_drops_with_command(
                    session_id,
                    Some("tool-use-1"),
                    &target_ids,
                    2,
                )
                .unwrap(),
            AppendOutcome {
                queued: 1,
                duplicate: false,
            }
        );
    }

    #[test]
    fn tags_mint_monotonically_and_channel1_appends_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let first = vec![
            TagMintInput {
                block_id: "m1#0".to_string(),
                kind: "message".to_string(),
                token_count: 11,
                source_bytes: b"message source".to_vec(),
            },
            TagMintInput {
                block_id: "m2#0".to_string(),
                kind: "tool_result".to_string(),
                token_count: 22,
                source_bytes: b"tool source".to_vec(),
            },
        ];
        let rows = store.mint_or_get_tags("ses", &first, 100).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.tag_number).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let second = vec![
            TagMintInput {
                block_id: "m1#0".to_string(),
                kind: "message".to_string(),
                token_count: 999,
                source_bytes: b"changed source must not overwrite".to_vec(),
            },
            TagMintInput {
                block_id: "m3#0".to_string(),
                kind: "tool_call".to_string(),
                token_count: 33,
                source_bytes: b"new source".to_vec(),
            },
        ];
        let rows = store.mint_or_get_tags("ses", &second, 200).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.tag_number).collect::<Vec<_>>(),
            vec![1, 3],
            "existing block keeps its tag; only new observations consume the next number"
        );
        let all = store.load_tags_for_session("ses").unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[0].token_count, 11,
            "token count is computed once at mint"
        );
        assert_eq!(
            all[0].source_bytes, b"message source",
            "pre-overlay provenance is immutable after the first mint"
        );
        let token_sum_ids = ["m1#0".to_string(), "m3#0".to_string()]
            .into_iter()
            .collect::<HashSet<_>>();
        assert_eq!(
            store
                .sum_tag_token_counts_for_blocks("ses", &token_sum_ids)
                .unwrap(),
            44
        );

        assert!(store
            .append_channel1_nudge(
                "ses",
                "m2#0",
                "\n\n<system-reminder>hi</system-reminder>",
                300
            )
            .unwrap());
        assert!(!store
            .append_channel1_nudge("ses", "m2#0", "different", 400)
            .unwrap());
        let appends = store.load_channel1_appends("ses").unwrap();
        assert_eq!(appends.len(), 1);
        assert_eq!(
            appends[0].reminder_text,
            "\n\n<system-reminder>hi</system-reminder>"
        );

        assert!(store.append_user_hint("ses", "m1#0", "", 500).unwrap());
        assert!(!store
            .append_user_hint("ses", "m1#0", "different", 600)
            .unwrap());
        assert!(store
            .append_user_hint(
                "ses",
                "m3#0",
                "\n\n<ctx-search-hint>hit</ctx-search-hint>",
                700
            )
            .unwrap());
        assert_eq!(
            store.load_user_hints("ses").unwrap(),
            vec![
                UserHintRow {
                    block_id: "m1#0".to_string(),
                    hint_text: String::new(),
                    created_at: 500,
                },
                UserHintRow {
                    block_id: "m3#0".to_string(),
                    hint_text: "\n\n<ctx-search-hint>hit</ctx-search-hint>".to_string(),
                    created_at: 700,
                },
            ]
        );
        assert!(!store.user_hint_frontier_open("ses", "m1#0", 1).unwrap());
        assert!(!store.user_hint_frontier_open("ses", "m2#0", 2).unwrap());
        assert!(store.user_hint_frontier_open("ses", "m4#0", 4).unwrap());
    }

    #[test]
    fn pass_trace_upserts_counts_and_caps_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let long_error = "é".repeat(2_500);

        store.trace_pass_received("trace", 11).unwrap();
        store.trace_pass_received("trace", 12).unwrap();
        store.trace_pass_rejected("trace", &long_error, 21).unwrap();
        store
            .trace_pass_rejected("trace", "second error", 22)
            .unwrap();
        store.trace_pass_completed("trace", 31).unwrap();

        let trace = store.load_pass_trace("trace").unwrap().unwrap();
        assert_eq!(trace.last_received_at_ms, 12);
        assert_eq!(trace.last_completed_at_ms, 31);
        assert_eq!(trace.last_reject_error.as_deref(), Some("second error"));
        assert_eq!(trace.last_reject_at_ms, Some(22));
        assert_eq!(trace.reject_count, 2);
        assert_eq!(trace.receive_count, 2);

        store
            .trace_pass_rejected("trace-cap", &long_error, 41)
            .unwrap();
        let capped = store.load_pass_trace("trace-cap").unwrap().unwrap();
        assert_eq!(
            capped.last_reject_error.as_ref().unwrap().chars().count(),
            2_000
        );
    }

    #[test]
    fn fresh_and_migrated_stores_have_latest_schema() {
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = McStore::open(&descriptor(fresh_dir.path())).unwrap();
        let fresh_has_table = fresh
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_pass_trace'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(fresh_has_table.as_deref(), Some("mc_pass_trace"));
        let fresh_has_import_table = fresh
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_state_imports'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(fresh_has_import_table.as_deref(), Some("mc_state_imports"));
        let fresh_has_hints_table = fresh
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_user_hints'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(fresh_has_hints_table.as_deref(), Some("mc_user_hints"));

        let migrated_dir = tempfile::tempdir().unwrap();
        let path = migrated_dir.path().join("store.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS cortexkit_schema_version (
                 namespace TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 applied_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (namespace, version)
             )",
            [],
        )
        .unwrap();
        for migration in MIGRATIONS.iter().filter(|migration| migration.version <= 8) {
            conn.execute_batch(migration.statements).unwrap();
            conn.execute(
                "INSERT INTO cortexkit_schema_version (namespace, version, applied_at_unix)
                 VALUES (?1, ?2, 0)",
                params![NS, migration.version],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO shadow_divergences
                 (session_id, pass_seq, class, ts_prefix, rs_prefix, normalizations,
                  ts_decision, rs_decision, state_hash, created_at)
             VALUES
                 ('shadow:test', 1, 'byte-mismatch', 'ts', 'rs', '[]', '{}', '{}', 'a', 1),
                 ('shadow:test', 1, 'quarantined', '', '', '[]', '{}', '{}', 'b', 2);",
        )
        .unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| (9..=14).contains(&migration.version))
        {
            conn.execute_batch(migration.statements).unwrap();
            conn.execute(
                "INSERT INTO cortexkit_schema_version (namespace, version, applied_at_unix)
                 VALUES (?1, ?2, 0)",
                params![NS, migration.version],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO mc_tags
                 (session_id, tag_number, block_id, kind, token_count, created_at_ms)
             VALUES ('legacy', 1, 'm1#0', 'message', 1, 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let migrated = McStore::open(&descriptor(migrated_dir.path())).unwrap();
        let migrated_has_table = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_pass_trace'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(migrated_has_table.as_deref(), Some("mc_pass_trace"));
        let migrated_has_import_table = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_state_imports'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(
            migrated_has_import_table.as_deref(),
            Some("mc_state_imports")
        );
        let migrated_has_hints_table = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'mc_user_hints'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(migrated_has_hints_table.as_deref(), Some("mc_user_hints"));
        let migrated_date_columns = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('mc_compartments')
                     WHERE name IN ('start_date', 'end_date')",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(migrated_date_columns, 2);
        let divergence_diagnostic_columns = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('shadow_divergences')
                     WHERE name IN ('first_diff_offset', 'ts_window', 'rs_window')",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(divergence_diagnostic_columns, 3);
        let tag_source_columns = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('mc_tags') WHERE name = 'source_bytes'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(tag_source_columns, 1);
        assert_eq!(
            migrated.load_tags_for_session("legacy").unwrap()[0].source_bytes,
            Vec::<u8>::new(),
            "migration preserves old tag rows with explicit unknown provenance"
        );
        let remaining_classes = migrated
            .inner
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT class FROM shadow_divergences ORDER BY id")?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(remaining_classes, vec!["byte-mismatch"]);
    }

    #[test]
    fn double_open_same_path_is_rejected_by_lease() {
        let dir = tempfile::tempdir().unwrap();
        let d = descriptor(dir.path());
        let _first = McStore::open(&d).unwrap();
        // Second live handle on the same database must be rejected (single-writer).
        assert!(McStore::open(&d).is_err());
    }

    fn import_compartment(
        sequence: i64,
        start_message: i64,
        end_message: i64,
        end_message_id: &str,
        p1: &str,
    ) -> StoredCompartment {
        StoredCompartment {
            sequence,
            start_message,
            end_message,
            end_message_id: end_message_id.to_string(),
            title: format!("imported {sequence}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    #[test]
    fn state_import_is_atomic_bootstrap_only_and_durably_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let compartments = vec![
            import_compartment(4, 1, 4, "m4#0", "first"),
            import_compartment(9, 5, 9, "m9#0", "second"),
        ];

        assert_eq!(
            store.preflight_state_import("fresh", "bundle-a").unwrap(),
            StateImportPreflight::Ready
        );
        let imported = store
            .commit_state_import("fresh", "bundle-a", &compartments, 123)
            .unwrap();
        assert_eq!(
            imported,
            StateImportResult {
                imported: 2,
                duplicate: false
            }
        );
        assert_eq!(store.load_compartments("fresh").unwrap(), compartments);
        let loaded = store.load("fresh").unwrap();
        assert!(loaded.core.boundary_id.is_empty());
        assert!(
            loaded.row_version.is_none(),
            "import leaves bootstrap INSERT to transform"
        );

        let malformed_retry = vec![import_compartment(1, 3, 2, "bad", "")];
        let duplicate = store
            .commit_state_import("fresh", "bundle-a", &malformed_retry, 999)
            .unwrap();
        assert_eq!(
            duplicate,
            StateImportResult {
                imported: 2,
                duplicate: true
            }
        );
        assert!(store.load("fresh").unwrap().row_version.is_none());
        assert!(matches!(
            store.commit_state_import("fresh", "bundle-b", &compartments, 999),
            Err(StateImportError::SessionNotEmpty)
        ));
        assert_eq!(store.load_compartments("fresh").unwrap(), compartments);

        store
            .commit("used", None, &CoreState::default(), &ModuleMeta::default())
            .unwrap();
        assert!(matches!(
            store.commit_state_import("used", "bundle-c", &compartments, 999),
            Err(StateImportError::SessionNotEmpty)
        ));
        assert!(store.load_compartments("used").unwrap().is_empty());
    }

    #[test]
    fn state_import_preflight_rejects_each_session_owned_state_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();

        let cache = store.load("cache").unwrap();
        store
            .commit("cache", None, &cache.core, &cache.meta)
            .unwrap();
        store
            .replace_compartments(
                "compartments",
                &[import_compartment(1, 1, 1, "m1#0", "summary")],
            )
            .unwrap();
        store
            .mint_or_get_tags(
                "tags",
                &[TagMintInput {
                    block_id: "m1#0".to_string(),
                    kind: "message".to_string(),
                    token_count: 1,
                    source_bytes: b"source".to_vec(),
                }],
                1,
            )
            .unwrap();
        store
            .append_pending_agent_drops("pending", &["m1#0".to_string()], 1)
            .unwrap();
        store
            .append_pending_agent_drops_with_command("ledger", Some("command"), &[], 1)
            .unwrap();
        store.append_user_hint("hints", "m1#0", "", 1).unwrap();

        for session_id in [
            "cache",
            "compartments",
            "tags",
            "pending",
            "ledger",
            "hints",
        ] {
            assert!(matches!(
                store.preflight_state_import(session_id, "bundle"),
                Err(StateImportError::SessionNotEmpty)
            ));
            assert!(matches!(
                store.commit_state_import(session_id, "bundle", &[], 2),
                Err(StateImportError::SessionNotEmpty)
            ));
        }
    }

    #[test]
    fn rejected_state_import_validation_leaves_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let invalid = vec![import_compartment(1, 3, 2, "m2#0", "bad")];

        let error = store
            .commit_state_import("fresh", "bundle-a", &invalid, 123)
            .unwrap_err();
        assert!(matches!(
            error,
            StateImportError::Validation(StateImportValidationError::RangeInvalid { .. })
        ));
        assert!(store.load_compartments("fresh").unwrap().is_empty());
        assert_eq!(
            store.preflight_state_import("fresh", "bundle-a").unwrap(),
            StateImportPreflight::Ready
        );
    }

    #[test]
    fn compartments_roundtrip_chronological_with_tiers_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        assert!(store.load_compartments("ses_a").unwrap().is_empty());

        let comps = vec![
            StoredCompartment {
                sequence: 1,
                start_message: 1,
                end_message: 9,
                title: "oldest legacy".into(),
                content: "U: flat body".into(),
                legacy: 1,
                importance: 50,
                created_at: 100,
                ..Default::default()
            },
            StoredCompartment {
                sequence: 2,
                start_message: 10,
                end_message: 19,
                start_date: Some("2026-01-02".into()),
                end_date: Some("2026-01-03".into()),
                title: "v2 row".into(),
                content: "P1 full".into(),
                p1: Some("P1 full".into()),
                p2: Some("P2 dense".into()),
                p3: Some("P3".into()),
                p4: None,
                importance: 80,
                episode_type: Some("design,feature".into()),
                legacy: 0,
                created_at: 200,
                ..Default::default()
            },
        ];
        store.replace_compartments("ses_a", &comps).unwrap();

        let read = store.load_compartments("ses_a").unwrap();
        assert_eq!(
            read, comps,
            "chronological round-trip incl NULL p4 + tiers + legacy"
        );
        assert_eq!(read[0].sequence, 1, "oldest first");

        // a wholesale replace fully supplants the prior set
        let replacement = vec![StoredCompartment {
            sequence: 1,
            title: "only".into(),
            content: "x".into(),
            importance: 50,
            ..Default::default()
        }];
        store.replace_compartments("ses_a", &replacement).unwrap();
        let read2 = store.load_compartments("ses_a").unwrap();
        assert_eq!(read2.len(), 1);
        assert_eq!(read2[0].title, "only");

        // distinct sessions are isolated
        assert!(store.load_compartments("ses_b").unwrap().is_empty());
    }

    fn insert_memory(
        store: &McStore,
        project: &str,
        id: i64,
        content: &str,
        importance: Option<i32>,
        status: &str,
        expires_at: Option<i64>,
    ) {
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_memories
                       (id, project_path, category, content, normalized_hash, importance,
                        status, expires_at, first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (?1,?2,'ARCHITECTURE',?3,?4,?5,?6,?7,0,0,0,0)",
                    params![
                        id,
                        project,
                        content,
                        format!("h{id}"),
                        importance,
                        status,
                        expires_at
                    ],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn active_memories_filter_order_and_frozen_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let proj = "git:proj";

        insert_memory(&store, proj, 1, "low active", Some(20), "active", None);
        insert_memory(&store, proj, 2, "high active", Some(90), "active", None);
        insert_memory(&store, proj, 3, "permanent", Some(50), "permanent", None);
        insert_memory(&store, proj, 4, "archived", Some(99), "archived", None); // excluded
        insert_memory(&store, proj, 5, "expired", Some(99), "active", Some(1000)); // expires at 1000
        insert_memory(&store, proj, 6, "other proj", Some(99), "active", None);
        // (re-key the last under a different project)
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "UPDATE mc_memories SET project_path = 'git:other' WHERE id = 6",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // cutoff AFTER the expiry → memory 5 excluded; archived + other-project excluded;
        // ordered importance desc (90, 50, 20).
        let read = store.load_active_memories(proj, 2000).unwrap();
        let ids: Vec<i64> = read.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![2, 3, 1],
            "active+permanent, expired excluded, importance desc"
        );

        // a cutoff BEFORE the expiry keeps memory 5 (frozen-cutoff determinism: the
        // caller controls the cutoff, not a live clock).
        let read_early = store.load_active_memories(proj, 500).unwrap();
        assert!(
            read_early.iter().any(|m| m.id == 5),
            "not-yet-expired at the earlier cutoff"
        );
    }

    fn log_mutation(store: &McStore, project: &str, kind: &str, target: i64, content: &str) {
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_memory_mutation_log
                       (project_path, mutation_type, target_memory_id, new_content, queued_at)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    params![project, kind, target, content],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mutation_render_coalesces_latest_wins_with_terminal_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let proj = "git:proj";

        // memory 10: two updates → latest-wins (single terminal correction).
        log_mutation(&store, proj, "update", 10, "v1"); // id 1
        log_mutation(&store, proj, "update", 10, "v2"); // id 2 (newer wins)
                                                        // memory 20: an archive then a later update → terminal (archive) outranks update.
        log_mutation(&store, proj, "archive", 20, ""); // id 3 terminal
        log_mutation(&store, proj, "update", 20, "resurrect?"); // id 4 must NOT win
                                                                // memory 30: in the log but NOT in the rendered manifest → excluded.
        log_mutation(&store, proj, "update", 30, "off-m0");

        let rendered = [10i64, 20];
        let projects = vec![proj.to_string()];
        let out = store
            .memory_mutations_for_render(&projects, 0, &rendered)
            .unwrap();

        assert_eq!(out.len(), 2, "one coalesced row per in-manifest target");
        let m10 = out.iter().find(|m| m.target_memory_id == 10).unwrap();
        assert_eq!(m10.new_content.as_deref(), Some("v2"), "latest update wins");
        let m20 = out.iter().find(|m| m.target_memory_id == 20).unwrap();
        assert_eq!(
            m20.mutation_type, "archive",
            "terminal outranks a later update"
        );
        assert!(
            !out.iter().any(|m| m.target_memory_id == 30),
            "off-manifest excluded"
        );

        // afterId cursor past id 2 → memory 10's updates fold out; 20's archive renders.
        let after = store
            .memory_mutations_for_render(&projects, 2, &rendered)
            .unwrap();
        assert!(
            !after.iter().any(|m| m.target_memory_id == 10),
            "folded updates excluded by cursor"
        );
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].target_memory_id, 20);

        // empty manifest → no corrections (nothing in m0 to correct).
        assert!(store
            .memory_mutations_for_render(&projects, 0, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn workspace_fingerprint_is_deterministic_and_membership_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        // a project in NO workspace → stable empty marker
        assert_eq!(store.workspace_fingerprint(own).unwrap(), "");

        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'ws','[\"CONSTRAINTS\",\"ARCHITECTURE\"]')",
                    [],
                )?;
                // insert members in NON-sorted order to prove the fingerprint canonicalizes
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at)
                     VALUES (1, ?1, 'foreign', '/f', 0), (1, ?2, 'own', '/o', 0)",
                    params![foreign, own],
                )?;
                Ok(())
            })
            .unwrap();

        // same membership → byte-identical across repeated reads (stability for an
        // unchanged workspace, so it never forces a needless re-render)
        let fp1 = store.workspace_fingerprint(own).unwrap();
        let fp2 = store.workspace_fingerprint(own).unwrap();
        assert_eq!(fp1, fp2, "stable workspace → stable fingerprint");
        assert!(!fp1.is_empty());
        // both members appear; the foreign member changes the marker (membership-sensitive)
        assert!(fp1.contains(own) && fp1.contains(foreign), "{fp1}");

        // removing the foreign member changes the fingerprint (a real membership change HARDs)
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "DELETE FROM mc_workspace_members WHERE project_path = ?1",
                    params![foreign],
                )?;
                Ok(())
            })
            .unwrap();
        let fp3 = store.workspace_fingerprint(own).unwrap();
        assert_ne!(fp1, fp3, "a real membership change must change the marker");
    }

    #[test]
    fn max_mutation_and_memory_ids_union_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        insert_memory(&store, own, 10, "a", Some(50), "active", None);
        insert_memory(&store, foreign, 25, "b", Some(50), "active", None);
        log_mutation(&store, own, "update", 10, "v2"); // id 1
        log_mutation(&store, foreign, "archive", 25, ""); // id 2

        // single-project sees only its own max
        assert_eq!(store.max_memory_id(&[own.to_string()]).unwrap(), 10);
        assert_eq!(store.max_memory_mutation_id(&[own.to_string()]).unwrap(), 1);
        // union spans both
        let union = vec![own.to_string(), foreign.to_string()];
        assert_eq!(store.max_memory_id(&union).unwrap(), 25);
        assert_eq!(store.max_memory_mutation_id(&union).unwrap(), 2);
        // empty inputs → 0 (no panic, no all-rows scan)
        assert_eq!(store.max_memory_id(&[]).unwrap(), 0);
        assert_eq!(store.max_memory_mutation_id(&[]).unwrap(), 0);
    }

    #[test]
    fn insert_memory_dedups_without_mutation_log_and_advances_memory_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        let paths = [project.to_string()];

        let id = store
            .insert_memory(insert_input(project, "CONSTRAINTS", "Use Rust", 10))
            .unwrap();
        assert_eq!(store.max_memory_id(&paths).unwrap(), id);
        assert_eq!(store.max_memory_mutation_id(&paths).unwrap(), 0);

        let duplicate = store
            .insert_memory(insert_input(project, "CONSTRAINTS", "  use   rust  ", 20))
            .unwrap();
        assert_eq!(
            duplicate, id,
            "normalized duplicate returns the existing id"
        );
        assert_eq!(store.max_memory_id(&paths).unwrap(), id);
        assert_eq!(store.max_memory_mutation_id(&paths).unwrap(), 0);
        assert_eq!(store.load_active_memories(project, 20).unwrap().len(), 1);
        assert_eq!(store.get_memory_full(id).unwrap().unwrap().seen_count, 2);
    }

    #[test]
    fn update_memory_content_advances_mutation_log_with_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        let id = store
            .insert_memory(insert_input(project, "ARCHITECTURE", "old", 1))
            .unwrap();
        let before = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();

        let updated = store
            .update_memory_content(project, id, "new", 2)
            .unwrap()
            .unwrap();
        assert_eq!(updated.content, "new");
        let after = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();
        assert!(after > before);
        let mutations = store
            .memory_mutations_for_render(&[project.to_string()], before, &[id])
            .unwrap();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].target_memory_id, id);
        assert_eq!(mutations[0].mutation_type, "update");
        assert_eq!(mutations[0].new_content.as_deref(), Some("new"));
    }

    #[test]
    fn archive_memory_advances_mutation_log_with_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        let id = store
            .insert_memory(insert_input(project, "CONSTRAINTS", "keep", 1))
            .unwrap();
        let before = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();

        let archived = store
            .archive_memory(project, id, Some("obsolete"), 2)
            .unwrap()
            .unwrap();
        assert_eq!(archived.status, "archived");
        assert!(archived
            .metadata_json
            .as_deref()
            .unwrap_or("")
            .contains("archive_reason"));
        let mutations = store
            .memory_mutations_for_render(&[project.to_string()], before, &[id])
            .unwrap();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].target_memory_id, id);
        assert_eq!(mutations[0].mutation_type, "archive");
    }

    #[test]
    fn merge_memories_logs_target_and_each_source_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        let target = store
            .insert_memory(insert_input(project, "CONSTRAINTS", "old target", 1))
            .unwrap();
        let source = store
            .insert_memory(insert_input(project, "CONSTRAINTS", "old source", 1))
            .unwrap();
        let before = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();

        let merged = store
            .merge_memories(project, target, &[source], "merged content", 2)
            .unwrap()
            .unwrap();
        assert_eq!(merged.content, "merged content");
        assert_eq!(merged.merged_from, Some(format!("[{target},{source}]")));
        let source_row = store.get_memory_full(source).unwrap().unwrap();
        assert_eq!(source_row.status, "archived");
        assert_eq!(source_row.superseded_by_memory_id, Some(target));

        let after = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();
        assert_eq!(after - before, 2, "target update + source supersede");
        let mutations = store
            .memory_mutations_for_render(&[project.to_string()], before, &[target, source])
            .unwrap();
        assert_eq!(mutations.len(), 2);
        assert!(mutations.iter().any(|m| {
            m.target_memory_id == target
                && m.mutation_type == "update"
                && m.new_content.as_deref() == Some("merged content")
        }));
        assert!(mutations.iter().any(|m| {
            m.target_memory_id == source
                && m.mutation_type == "superseded"
                && m.superseded_by_id == Some(target)
        }));
    }

    #[test]
    fn mutation_render_spans_workspace_union() {
        // a foreign member's shared memory (in the manifest) updates → its correction
        // must render across the whole workspace union, not just the own project; a
        // single-project query would miss it (the foreign update would never supersede).
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        log_mutation(&store, own, "update", 100, "own-updated"); // id 1
        log_mutation(&store, foreign, "update", 200, "foreign-updated"); // id 2

        // manifest holds BOTH (the union baseline rendered own-100 + foreign-shared-200)
        let rendered = [100i64, 200];
        let single = store
            .memory_mutations_for_render(&[own.to_string()], 0, &rendered)
            .unwrap();
        assert_eq!(single.len(), 1, "single-project misses the foreign update");
        assert_eq!(single[0].target_memory_id, 100);

        let union = store
            .memory_mutations_for_render(&[own.to_string(), foreign.to_string()], 0, &rendered)
            .unwrap();
        let targets: Vec<i64> = union.iter().map(|m| m.target_memory_id).collect();
        assert!(
            targets.contains(&100) && targets.contains(&200),
            "union supersedes both: {targets:?}"
        );
    }

    #[test]
    fn active_user_memories_ordered_promoted_then_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let insert = |id: i64, content: &str, status: &str, promoted: i64| {
            store
                .inner
                .with_conn_fenced(|tx| {
                    tx.execute(
                        "INSERT INTO mc_user_memories (id, content, status, promoted_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![id, content, status, promoted],
                    )?;
                    Ok(())
                })
                .unwrap();
        };
        // two share promoted_at=100 → id breaks the tie deterministically (3 before 4).
        insert(1, "first", "active", 50);
        insert(4, "tie-later-id", "active", 100);
        insert(3, "tie-earlier-id", "active", 100);
        insert(2, "archived", "archived", 10); // status != active → excluded from the result

        let got = store.load_active_user_memories().unwrap();
        assert_eq!(got, vec!["first", "tie-earlier-id", "tie-later-id"]);
    }

    #[test]
    fn workspace_union_shares_foreign_only_in_shared_categories() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        // own: id1 CONSTRAINTS + id2 ARCHITECTURE (both visible to own). foreign: id3
        // CONSTRAINTS (shared) + id4 ARCHITECTURE (NOT shared). insert_memory defaults
        // category=ARCHITECTURE, so insert all four then UPDATE ids 1 and 3 to CONSTRAINTS.
        insert_memory(&store, own, 1, "own constraint", Some(70), "active", None);
        insert_memory(&store, own, 2, "own arch", Some(90), "active", None);
        insert_memory(
            &store,
            foreign,
            3,
            "foreign shared",
            Some(80),
            "active",
            None,
        );
        insert_memory(
            &store,
            foreign,
            4,
            "foreign secret",
            Some(99),
            "active",
            None,
        );
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "UPDATE mc_memories SET category='CONSTRAINTS' WHERE id IN (1,3)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // build a workspace with share_categories=["CONSTRAINTS"], members own+foreign.
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'ws','[\"CONSTRAINTS\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at)
                     VALUES (1, ?1, 'own', '/own', 0), (1, ?2, 'svc-foreign', '/foreign', 0)",
                    params![own, foreign],
                )?;
                Ok(())
            })
            .unwrap();

        let membership = store.resolve_workspace_membership(own).unwrap().unwrap();
        assert_eq!(membership.own_identity, own);
        assert_eq!(membership.share_categories, vec!["CONSTRAINTS"]);
        assert_eq!(
            membership
                .display_name_by_path
                .get(foreign)
                .map(String::as_str),
            Some("svc-foreign")
        );

        let union = store.load_workspace_union_memories(&membership, 0).unwrap();
        let ids: Vec<i64> = union.iter().map(|m| m.id).collect();
        // own sees BOTH its own (1 CONSTRAINTS, 2 ARCHITECTURE); foreign only the shared
        // CONSTRAINTS (3) — NOT the foreign ARCHITECTURE (4, the security boundary).
        assert!(
            ids.contains(&1) && ids.contains(&2),
            "own sees all own: {ids:?}"
        );
        assert!(
            ids.contains(&3),
            "foreign shared CONSTRAINTS visible: {ids:?}"
        );
        assert!(
            !ids.contains(&4),
            "foreign non-shared ARCHITECTURE must NOT leak: {ids:?}"
        );

        // a project in NO workspace → None (single-project fast path)
        assert!(store
            .resolve_workspace_membership("git:loner")
            .unwrap()
            .is_none());
    }

    #[test]
    fn append_compartments_preserves_existing_rows_and_assigns_tail_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let c1 = StoredCompartment {
            sequence: 1,
            start_message: 1,
            end_message: 2,
            end_message_id: "m2".into(),
            title: "old-1".into(),
            content: "old one".into(),
            ..Default::default()
        };
        let c2 = StoredCompartment {
            sequence: 2,
            start_message: 3,
            end_message: 4,
            end_message_id: "m4".into(),
            title: "old-2".into(),
            content: "old two".into(),
            ..Default::default()
        };
        store
            .replace_compartments("ses", &[c1.clone(), c2.clone()])
            .unwrap();

        let appended = StoredCompartment {
            sequence: 99,
            start_message: 5,
            end_message: 6,
            end_message_id: "m6".into(),
            title: "new".into(),
            content: "new tail".into(),
            ..Default::default()
        };
        store.append_compartments("ses", &[appended]).unwrap();

        let rows = store.load_compartments("ses").unwrap();
        let seqs: Vec<i64> = rows.iter().map(|c| c.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(rows[0].title, c1.title);
        assert_eq!(rows[1].title, c2.title);
        assert_eq!(rows[2].title, "new");
    }

    #[test]
    fn promote_facts_exact_dedup_skips_duplicates_and_advances_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .seed_memory(1, "git:proj", "ARCHITECTURE", "already active", 70)
            .unwrap();
        let before = store.max_memory_id(&["git:proj".to_string()]).unwrap();

        let promoted = store
            .promote_facts(
                "git:proj",
                &[
                    FactCandidate {
                        category: "ARCHITECTURE".into(),
                        content: "already active".into(),
                        ..Default::default()
                    },
                    FactCandidate {
                        category: "ARCHITECTURE".into(),
                        content: "new fact".into(),
                        importance: Some(80),
                        ..Default::default()
                    },
                    FactCandidate {
                        category: "CONSTRAINTS".into(),
                        content: "new fact".into(),
                        ..Default::default()
                    },
                ],
            )
            .unwrap();

        assert_eq!(before, 1);
        assert_eq!(promoted.len(), 1, "duplicate active content is skipped");
        assert_eq!(promoted[0].content, "new fact");
        let after = store.max_memory_id(&["git:proj".to_string()]).unwrap();
        assert_eq!(after, promoted[0].memory_id);
        assert!(after > before, "additive insert advances max_memory_id");
        let active = store.load_active_memories("git:proj", 0).unwrap();
        let contents: Vec<&str> = active.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["new fact", "already active"]);
    }

    fn publishing_meta() -> ModuleMeta {
        ModuleMeta {
            historian: HistorianDurableState {
                state: HistorianPhase::Publishing,
                firing_seq: 7,
                chunk_range: Some(HistorianChunkRange {
                    from_ordinal: 10,
                    to_ordinal: 20,
                }),
                chunk_fingerprint: "fp".into(),
                producer_session_id: Some("producer-session".into()),
                producer_run_id: Some("run-1".into()),
                fired_at_ms: Some(123),
                expected_revert_epoch: 0,
                failure_backoff_at_ms: Some(456),
                last_failure: None,
                last_no_fire: None,
            },
            ..Default::default()
        }
    }

    fn publish_predicate() -> HistorianPublishPredicate {
        HistorianPublishPredicate {
            firing_seq: 7,
            producer_run_id: "run-1".into(),
            chunk_fingerprint: "fp".into(),
        }
    }

    fn publish_compartment() -> StoredCompartment {
        StoredCompartment {
            start_message: 10,
            end_message: 20,
            end_message_id: "m20".into(),
            title: "published".into(),
            content: "published summary".into(),
            ..Default::default()
        }
    }

    #[test]
    fn publish_historian_chunk_is_cas_gated_and_double_publish_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();
        let loaded = store.load("ses").unwrap();
        let expected = loaded.row_version;

        let first = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[FactCandidate {
                    category: "ARCHITECTURE".into(),
                    content: "published fact".into(),
                    ..Default::default()
                }],
                publication_floor_ordinal: 21,
                chunk_transcript: None,
            })
            .unwrap();
        assert_eq!(first.row_version, 2);

        let err = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, HistorianPublishError::CasConflict { .. }),
            "second racing publisher must hit the row-version CAS: {err:?}"
        );
        assert_eq!(store.load_compartments("ses").unwrap().len(), 1);
        let loaded = store.load("ses").unwrap();
        assert_eq!(loaded.meta.historian.state, HistorianPhase::Idle);
        assert_eq!(loaded.meta.historian.firing_seq, 7);
        assert_eq!(loaded.meta.publication_floor_ordinal, Some(21));
        assert_eq!(store.max_memory_id(&["git:proj".to_string()]).unwrap(), 1);
    }

    #[test]
    fn publish_historian_chunk_persists_transcript_inside_cas() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();
        let expected = store.load("ses").unwrap().row_version;
        store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: Some("U: hello\nA: world"),
            })
            .unwrap();

        let rows = store
            .load_chunk_transcripts_for_range("ses", 10, 21)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].compartment_seq, 1);
        assert_eq!(rows[0].transcript.as_deref(), Some("U: hello\nA: world"));
    }

    #[test]
    fn publish_historian_chunk_cas_conflict_leaves_no_transcript_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();
        let err = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: Some(99),
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: Some("U: orphan"),
            })
            .unwrap_err();
        assert!(matches!(err, HistorianPublishError::CasConflict { .. }));
        assert!(store
            .load_chunk_transcripts_for_range("ses", 10, 21)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn oversized_chunk_transcript_is_evicted_as_unrecoverable() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();
        let transcript = (0..50_000)
            .map(|i| format!("{:x}", md5::compute(i.to_string())))
            .collect::<String>();
        assert!(
            compress_transcript(&transcript).unwrap().len() > MAX_CHUNK_TRANSCRIPT_COMPRESSED_BYTES
        );
        let expected = store.load("ses").unwrap().row_version;
        store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: Some(&transcript),
            })
            .unwrap();
        assert!(store
            .load_chunk_transcripts_for_range("ses", 10, 21)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn notes_crud_pagination_dismiss_resolution_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let first = store
            .insert_note(NoteInput {
                project_path: "git:proj",
                session_id: "ses",
                content: "Revisit frobnicator later",
                surface_condition: Some("when release tag advances"),
                anchor_block_id: Some("m9#0"),
                now_ms: 10,
            })
            .unwrap();
        let second = store
            .insert_note(NoteInput {
                project_path: "git:proj",
                session_id: "ses",
                content: "Check pagination",
                surface_condition: None,
                anchor_block_id: None,
                now_ms: 20,
            })
            .unwrap();
        assert_eq!(
            store.read_notes("git:proj", "ses", 1, 0).unwrap()[0].id,
            second.id
        );
        assert_eq!(
            store.read_notes("git:proj", "ses", 1, 1).unwrap()[0].id,
            first.id
        );
        store
            .update_note_content(
                "git:proj",
                "ses",
                first.id,
                "Revisit updated frobnicator",
                30,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .search_notes_like("git:proj", "ses", "updated")
                .unwrap()[0]
                .id,
            first.id
        );
        let dismissed = store
            .dismiss_note("git:proj", "ses", first.id, Some("done in v2"), 40)
            .unwrap()
            .unwrap();
        assert_eq!(dismissed.status, "dismissed");
        assert!(dismissed.content.contains("done in v2"));
        assert_eq!(store.read_notes("git:proj", "ses", 25, 0).unwrap().len(), 1);
        assert!(store
            .search_notes_like("git:other", "ses", "pagination")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn publish_historian_chunk_rejects_wrong_fingerprint_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();
        let expected = store.load("ses").unwrap().row_version;
        let wrong = HistorianPublishPredicate {
            chunk_fingerprint: "different".into(),
            ..publish_predicate()
        };

        let err = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &wrong,
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[FactCandidate {
                    category: "ARCHITECTURE".into(),
                    content: "should not insert".into(),
                    ..Default::default()
                }],
                publication_floor_ordinal: 21,
                chunk_transcript: None,
            })
            .unwrap_err();
        assert!(matches!(err, HistorianPublishError::StateMismatch { .. }));
        assert!(store.load_compartments("ses").unwrap().is_empty());
        assert_eq!(store.max_memory_id(&["git:proj".to_string()]).unwrap(), 0);
        assert_eq!(
            store.load("ses").unwrap().meta.historian.state,
            HistorianPhase::Publishing
        );
    }

    #[test]
    fn publish_historian_chunk_fails_loud_from_non_publish_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &ModuleMeta::default())
            .unwrap();
        let expected = store.load("ses").unwrap().row_version;

        let err = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: None,
            })
            .unwrap_err();
        assert!(
            matches!(err, HistorianPublishError::InvalidState { ref state } if state == "idle"),
            "idle state must fail loudly: {err:?}"
        );
        assert!(store.load_compartments("ses").unwrap().is_empty());
    }
    fn recut_comp(seq: i64, start: i64, end: i64, end_id: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            start_message_id: format!("m{start}#0"),
            end_message_id: end_id.to_string(),
            title: format!("C{seq}"),
            content: format!("summary {seq}"),
            p1: Some(format!("summary {seq}")),
            importance: 50,
            ..Default::default()
        }
    }

    #[test]
    fn truncate_compartments_for_revert_deletes_suffix_and_bumps_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let meta = ModuleMeta {
            coverage_ordinal: Some(3),
            folded_compartment_seq: 3,
            ..Default::default()
        };
        let rv = store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        store
            .replace_compartments(
                "ses",
                &[
                    recut_comp(1, 1, 1, "a#0"),
                    recut_comp(2, 2, 2, "b#0"),
                    recut_comp(3, 3, 3, "c#0"),
                ],
            )
            .unwrap();

        let outcome = store
            .truncate_compartments_for_revert("ses", 1, Some(rv))
            .unwrap();
        assert_eq!(outcome.revert_epoch, 1);
        assert_eq!(outcome.row_version, rv + 1);
        assert!(outcome
            .last_recut
            .as_deref()
            .unwrap()
            .contains("dropped seq 2..3"));
        let loaded = store.load("ses").unwrap();
        assert_eq!(loaded.meta.revert_epoch, 1);
        assert_eq!(loaded.meta.last_recut, outcome.last_recut);
        let compartments = store.load_compartments("ses").unwrap();
        assert_eq!(compartments.len(), 1);
        assert_eq!(compartments[0].sequence, 1);

        let no_op = store
            .truncate_compartments_for_revert("ses", 1, Some(outcome.row_version))
            .unwrap();
        assert_eq!(no_op.revert_epoch, 1);
        assert_eq!(no_op.row_version, outcome.row_version);
        assert_eq!(store.load_compartments("ses").unwrap().len(), 1);
    }

    #[test]
    fn assembly_snapshot_reads_compartments_and_revert_epoch_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let meta = ModuleMeta {
            revert_epoch: 4,
            ..Default::default()
        };
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        store
            .replace_compartments("ses", &[recut_comp(1, 1, 1, "a#0")])
            .unwrap();

        let snapshot = store.load_historian_assembly_snapshot("ses").unwrap();
        assert_eq!(snapshot.revert_epoch, 4);
        assert_eq!(snapshot.compartments.len(), 1);
        assert_eq!(snapshot.compartments[0].end_message_id, "a#0");
    }

    #[test]
    fn publish_historian_chunk_rejects_recut_epoch_mismatch_as_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let mut meta = publishing_meta();
        meta.revert_epoch = 1;
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        let expected = store.load("ses").unwrap().row_version;

        let err = store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: expected,
                expected_revert_epoch: 0,
                predicate: &publish_predicate(),
                project_path: "git:proj",
                compartments: &[publish_compartment()],
                facts: &[],
                publication_floor_ordinal: 21,
                chunk_transcript: None,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            HistorianPublishError::CasConflict {
                reason: Some(ref reason),
                ..
            } if reason == "revert epoch mismatch (session was re-cut mid-firing)"
        ));
        assert!(store.load_compartments("ses").unwrap().is_empty());
        assert_eq!(
            store.load("ses").unwrap().meta.historian.state,
            HistorianPhase::Publishing
        );
    }
}

#[cfg(test)]
mod shadow_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend};

    fn store(dir: &std::path::Path) -> McStore {
        McStore::open(&StorageDescriptor {
            module_id: "magic-context-test".to_string(),
            storage_namespace: "mc_cache".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap()
    }

    fn comp(sequence: i64, end: i64, end_id: &str) -> StoredCompartment {
        StoredCompartment {
            sequence,
            start_message: 0,
            end_message: end,
            start_message_id: "a#0".to_string(),
            end_message_id: end_id.to_string(),
            title: "c".to_string(),
            content: "p1".to_string(),
            p1: Some("p1".to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    fn memory(id: i64, content: &str) -> ShadowMemoryRow {
        ShadowMemoryRow {
            id,
            project_path: "shadow:real".to_string(),
            category: "CONSTRAINTS".to_string(),
            content: content.to_string(),
            normalized_hash: compute_normalized_memory_hash(content),
            importance: Some(70),
            scope: "project".to_string(),
            status: "active".to_string(),
            verification_status: "unverified".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn shadow_workspace_union_stays_isolated_from_real_project_queries() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:workspace-session";
        let foreign = "shadow:workspace-session:member:foreign";
        let workspace = ShadowWorkspaceRow {
            name: "shadow-workspace-isolation".to_string(),
            share_categories: vec!["CONSTRAINTS".to_string()],
            members: vec![
                ShadowWorkspaceMemberRow {
                    project_path: session.to_string(),
                    display_name: "owner".to_string(),
                    display_path: "/real/owner".to_string(),
                },
                ShadowWorkspaceMemberRow {
                    project_path: foreign.to_string(),
                    display_name: "foreign".to_string(),
                    display_path: "/real/foreign".to_string(),
                },
            ],
        };
        let mut own = memory(1, "own architecture");
        own.project_path = session.to_string();
        own.category = "ARCHITECTURE".to_string();
        let mut shared = memory(2, "foreign constraint");
        shared.project_path = foreign.to_string();
        let mut private = memory(3, "foreign preference");
        private.project_path = foreign.to_string();
        private.category = "PREFERENCES".to_string();

        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &[],
                memories: &[own, shared, private],
                memory_mutations: &[],
                workspace: Some(&workspace),
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();

        let membership = store
            .resolve_workspace_membership(session)
            .unwrap()
            .expect("shadow workspace membership");
        assert!(membership
            .union_identities
            .iter()
            .all(|path| path.starts_with(SHADOW_SESSION_PREFIX)));
        let visible = store.load_workspace_union_memories(&membership, 0).unwrap();
        assert_eq!(
            visible.iter().map(|memory| memory.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(store
            .resolve_workspace_membership("/real/foreign")
            .unwrap()
            .is_none());
        assert!(store
            .load_active_memories("/real/foreign", 0)
            .unwrap()
            .is_empty());

        store.reset_shadow_session(session, session).unwrap();
        assert!(store
            .resolve_workspace_membership(session)
            .unwrap()
            .is_none());
        assert!(store.load_active_memories(foreign, 0).unwrap().is_empty());
    }

    #[test]
    fn shadow_state_sync_is_generation_and_zero_seq_gated() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:real";
        let project = "shadow:real";
        let compartments = vec![comp(0, 0, "a#0")];
        let memories = vec![memory(1, "remember zero")];
        let mutations = vec![ShadowMemoryMutationRow {
            project_path: project.to_string(),
            mutation: StoredMemoryMutation {
                id: 0,
                mutation_type: "update".to_string(),
                target_memory_id: 1,
                new_content: Some("remember one".to_string()),
                ..Default::default()
            },
        }];

        let applied = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: project,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &compartments,
                memories: &memories,
                memory_mutations: &mutations,
                workspace: None,
                last_todo_state: Some("[]".to_string()),
                acked_watermarks: serde_json::json!({"seq": 0}),
            })
            .unwrap();
        assert_eq!(applied.shadow_seq, 1);
        let loaded = store.load(session).unwrap();
        assert_eq!(loaded.meta.shadow_generation, 0);
        assert_eq!(loaded.meta.shadow_seq, 1);
        assert_eq!(loaded.meta.last_todo_state.as_deref(), Some("[]"));
        assert_eq!(store.load_compartments(session).unwrap()[0].sequence, 0);
        assert_eq!(store.load_active_memories(project, 0).unwrap()[0].id, 1);
        assert_eq!(
            store
                .max_memory_mutation_id(&[project.to_string()])
                .unwrap(),
            0
        );

        let seq_reject = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: project,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap_err();
        assert!(matches!(
            seq_reject,
            ShadowStateSyncError::SeqMismatch {
                expected: 0,
                found: 1
            }
        ));

        let reset = store.reset_shadow_session(session, project).unwrap();
        assert_eq!(reset.shadow_generation, 1);
        assert_eq!(reset.shadow_seq, 0);
        assert!(store.load_compartments(session).unwrap().is_empty());
        assert!(store.load_active_memories(project, 0).unwrap().is_empty());

        let stale_generation = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: project,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap_err();
        assert!(matches!(
            stale_generation,
            ShadowStateSyncError::GenerationMismatch {
                expected: 0,
                found: 1
            }
        ));
    }

    #[test]
    fn assembled_paged_seed_without_reset_retains_omitted_compartments() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:reset-required";
        let initial = vec![comp(0, 0, "first#0"), comp(1, 1, "stale#0")];
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &initial,
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();

        // A completed paged seed reaches the store as one assembled request. Omitting
        // the existing sequence-1 compartment here preserves it unless reset ran first.
        let replacement = vec![comp(0, 0, "replacement#0")];
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 1,
                seed_boundary_id: None,
                compartments: &replacement,
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(
            store
                .load_compartments(session)
                .unwrap()
                .iter()
                .map(|compartment| compartment.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "state_sync upserts and cannot prove completeness without a prior reset"
        );

        store.reset_shadow_session(session, session).unwrap();
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 1,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &replacement,
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(store.load_compartments(session).unwrap().len(), 1);
    }

    #[test]
    fn shadow_seed_boundary_mismatch_rejects_without_partial_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:stale-boundary";
        let compartments = vec![comp(7, 12, "tail#2")];

        let error = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: Some("tail#1"),
                compartments: &compartments,
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ShadowStateSyncError::InvalidSeedBoundary { .. }
        ));
        let loaded = store.load(session).unwrap();
        assert_eq!(loaded.meta.shadow_seq, 0);
        assert!(loaded.core.boundary_id.is_empty());
        assert!(store.load_compartments(session).unwrap().is_empty());
    }

    #[test]
    fn shadow_divergence_quarantines_until_reset() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:real";
        let project = "shadow:real";
        let reset = store.reset_shadow_session(session, project).unwrap();

        let write = store
            .record_shadow_divergence(ShadowDivergenceRecord {
                session_id: session,
                shadow_generation: reset.shadow_generation,
                pass_seq: 0,
                class: "byte-mismatch",
                first_mid: Some("m0"),
                first_block: Some("0"),
                first_field: Some("content"),
                ts_prefix: "ts",
                rs_prefix: "rs",
                first_diff_offset: Some(3),
                ts_window: "ts-window",
                rs_window: "rs-window",
                normalizations_json: "[]",
                ts_decision_json: "{}",
                rs_decision_json: "{}",
                state_hash: "hash",
                created_at_ms: 7,
                quarantine: true,
            })
            .unwrap();
        assert!(write.quarantined);
        assert!(store.load(session).unwrap().meta.shadow_quarantined);
        let rows = store.load_shadow_divergences(session).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].first_diff_offset, Some(3));
        assert_eq!(rows[0].ts_window, "ts-window");
        assert_eq!(rows[0].rs_window, "rs-window");

        let repeated = store
            .record_shadow_divergence(ShadowDivergenceRecord {
                session_id: session,
                shadow_generation: reset.shadow_generation,
                pass_seq: 1,
                class: "quarantined",
                first_mid: None,
                first_block: None,
                first_field: None,
                ts_prefix: "",
                rs_prefix: "",
                first_diff_offset: None,
                ts_window: "",
                rs_window: "",
                normalizations_json: "[]",
                ts_decision_json: "{}",
                rs_decision_json: "{}",
                state_hash: "hash",
                created_at_ms: 8,
                quarantine: false,
            })
            .unwrap();
        assert!(repeated.quarantined);
        assert_eq!(store.load_shadow_divergences(session).unwrap().len(), 1);
        assert_eq!(
            store
                .load(session)
                .unwrap()
                .meta
                .shadow_quarantined_pass_count,
            1
        );

        let reset = store.reset_shadow_session(session, project).unwrap();
        assert_eq!(reset.shadow_generation, 2);
        let loaded = store.load(session).unwrap();
        assert!(!loaded.meta.shadow_quarantined);
        assert_eq!(loaded.meta.shadow_quarantined_pass_count, 0);
        assert_eq!(store.load_shadow_divergences(session).unwrap().len(), 1);
    }
}
