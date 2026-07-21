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

use cortexkit_cache_core::{CoreState, DurabilityClass, FrozenUnit};
use cortexkit_store::{open_sqlite, Migration, SqliteStore, StoreError};
use cortexkit_store_types::StorageDescriptor;
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use rusqlite::{functions::FunctionFlags, params, types::Value as SqlValue, OptionalExtension};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub type ProviderExtras = BTreeMap<String, BTreeMap<String, Value>>;

/// Canonicalize a filesystem route root before using it as lineage state.
///
/// Transform and facade lanes can observe the same directory through different spellings when
/// a project is reached through a symlink. Comparing those spellings as strings would make a
/// valid session look unresolved, so filesystem roots share this boundary normalization. A root
/// may have been removed by the time a request arrives; retain the input spelling in that case
/// instead of turning canonicalization into a request failure.
pub fn canonical_root(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Canonical foreign-memory visibility predicate used by module SQL consumers.
/// The plugin mirrors this literal to keep cache visibility and workspace policy aligned.
pub const FOREIGN_VISIBLE_SQL: &str = "status IN ('active','permanent') AND (expires_at IS NULL OR expires_at > :now_ms) AND shareable = 1 AND scope IN ('project','ecosystem','universe') AND category IN (SELECT value FROM json_each(:share_categories)) AND project_path IN (SELECT project_path FROM mc_workspace_members WHERE workspace_id = :workspace_id) AND project_path <> :reader_project";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub errored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
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
    // Error-flagged content arrays keep their own variant so failure state survives
    // decode/encode; errors are output variants here, never a sibling flag.
    ErrorContent { blocks: Vec<ResultBlock> },
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
            CkOutputKind::ErrorContent { .. } => "error_content",
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

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

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
    Migration {
        version: 19,
        // Overlay decisions use an ordinal watermark rather than observation order so a
        // restored older message cannot first-apply bytes after a newer pass. Empty marker
        // text freezes a no-marker decision. Wrapup command rows make response-loss retries
        // replay the original terminal result without driving the historian again.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_overlay_frontiers (
            session_id        TEXT PRIMARY KEY,
            max_seen_ordinal  INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS mc_temporal_marks (
            session_id   TEXT NOT NULL,
            block_id     TEXT NOT NULL,
            marker_text  TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (session_id, block_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_temporal_marks_session_created
            ON mc_temporal_marks(session_id, created_at, block_id);

        CREATE TABLE IF NOT EXISTS mc_wrapup_commands (
            session_id   TEXT NOT NULL,
            command_id   TEXT NOT NULL,
            disposition  TEXT NOT NULL
                CHECK (disposition IN ('completed', 'nothing_to_compact', 'failed')),
            rounds       INTEGER NOT NULL,
            summary      TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (session_id, command_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_wrapup_commands_session_created
            ON mc_wrapup_commands(session_id, created_at, command_id);
    ",
    },
    Migration {
        version: 20,
        // User-profile rows belong to the shadow project namespace. Before 2026-07-16
        // 17:14Z, missing profile rows and unsendable oversized items were known false
        // positives. Delete those diagnostic rows because they predate both fixes.
        statements: "
        CREATE TABLE IF NOT EXISTS shadow_user_profile (
            shadow_project_path  TEXT NOT NULL,
            profile_index        INTEGER NOT NULL,
            content              TEXT NOT NULL,
            PRIMARY KEY (shadow_project_path, profile_index)
        );
        CREATE INDEX IF NOT EXISTS idx_shadow_user_profile_project
            ON shadow_user_profile(shadow_project_path, profile_index);
        DELETE FROM shadow_divergences
         WHERE session_id LIKE 'shadow:%'
           AND created_at < 1784222040000;
    ",
    },
    Migration {
        version: 21,
        // A command owns one first-application window. Its protected remainder rides a
        // later byte-changing pass instead of becoming a sequence of smaller busts.
        statements: "
        ALTER TABLE pending_agent_drops ADD COLUMN command_id TEXT;
        ALTER TABLE mc_reduce_command_ledger ADD COLUMN first_applied_at_ms INTEGER;
        CREATE INDEX IF NOT EXISTS idx_pending_agent_drops_command
            ON pending_agent_drops(session_id, command_id, id);
    ",
    },
    Migration {
        version: 22,
        // Recomp command outcomes are kept separately from the wrapup ledger because the
        // two operations have different disposition vocabularies but identical replay needs.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_recomp_commands (
            session_id   TEXT NOT NULL,
            command_id   TEXT NOT NULL,
            disposition  TEXT NOT NULL CHECK (disposition IN ('started', 'already_in_progress', 'nothing_to_do')),
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (session_id, command_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_recomp_commands_session_created
            ON mc_recomp_commands(session_id, created_at, command_id);
    ",
    },
    Migration {
        version: 23,
        // Authority and feed rows are storage-plane state. The source key lets a seed be
        // retried after a crash without allocating a second module row for one context row.
        // Feed triggers are deliberately the only append mechanism: direct SQL, facade ops,
        // and future writers all receive the same complete snapshot automatically.
        statements: r#"
        ALTER TABLE mc_memories ADD COLUMN context_store_uuid TEXT;
        ALTER TABLE mc_memories ADD COLUMN context_row_id INTEGER;
        ALTER TABLE mc_notes ADD COLUMN context_store_uuid TEXT;
        ALTER TABLE mc_notes ADD COLUMN context_row_id INTEGER;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_mc_memories_context_source
            ON mc_memories(context_store_uuid, context_row_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_mc_notes_context_source
            ON mc_notes(context_store_uuid, context_row_id);

        CREATE TABLE IF NOT EXISTS mc_changefeed (
            feed_seq            INTEGER PRIMARY KEY AUTOINCREMENT,
            domain              TEXT NOT NULL CHECK (domain IN ('memories', 'notes')),
            op                  TEXT NOT NULL CHECK (op IN ('insert', 'update', 'tombstone')),
            module_row_id       INTEGER NOT NULL,
            full_row_snapshot   JSON NOT NULL,
            content_hash        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_mc_changefeed_domain_seq
            ON mc_changefeed(domain, feed_seq);

        CREATE TABLE IF NOT EXISTS mc_authority (
            context_store_uuid TEXT NOT NULL,
            project            TEXT NOT NULL,
            domain             TEXT NOT NULL CHECK (domain IN ('memories', 'notes')),
            state               TEXT NOT NULL CHECK (state IN ('TS', 'PREPARING', 'MODULE', 'DRAINING')),
            generation         INTEGER NOT NULL DEFAULT 0,
            captured_upper_bound INTEGER,
            drain_generation   INTEGER,
            drain_cursor       INTEGER NOT NULL DEFAULT 0,
            step_seed          INTEGER NOT NULL DEFAULT 0,
            step_memories      INTEGER NOT NULL DEFAULT 0,
            step_notes         INTEGER NOT NULL DEFAULT 0,
            step_compartments  INTEGER NOT NULL DEFAULT 0,
            step_reconcile     INTEGER NOT NULL DEFAULT 0,
            step_verify        INTEGER NOT NULL DEFAULT 0,
            step_flip          INTEGER NOT NULL DEFAULT 0,
            coordinator_lease TEXT,
            lease_expires_at  INTEGER,
            checksum_expected TEXT,
            checksum_actual   TEXT,
            checksum_ok       INTEGER,
            PRIMARY KEY (context_store_uuid, project, domain)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_authority_project
            ON mc_authority(context_store_uuid, project, state);

        DROP TRIGGER IF EXISTS mc_memories_feed_insert;
        DROP TRIGGER IF EXISTS mc_memories_feed_update;
        DROP TRIGGER IF EXISTS mc_memories_feed_delete;
        CREATE TRIGGER mc_memories_feed_insert AFTER INSERT ON mc_memories BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('memories', 'insert', NEW.id,
                json_object(
                    'id', NEW.id, 'project_path', NEW.project_path, 'category', NEW.category,
                    'content', NEW.content, 'normalized_hash', NEW.normalized_hash,
                    'importance', NEW.importance, 'scope', NEW.scope, 'shareable', NEW.shareable,
                    'source_session_id', NEW.source_session_id, 'source_type', NEW.source_type,
                    'seen_count', NEW.seen_count, 'retrieval_count', NEW.retrieval_count,
                    'first_seen_at', NEW.first_seen_at, 'created_at', NEW.created_at,
                    'updated_at', NEW.updated_at, 'last_seen_at', NEW.last_seen_at,
                    'last_retrieved_at', NEW.last_retrieved_at, 'status', NEW.status,
                    'expires_at', NEW.expires_at, 'verification_status', NEW.verification_status,
                    'verified_at', NEW.verified_at, 'classified_at', NEW.classified_at,
                    'superseded_by_memory_id', NEW.superseded_by_memory_id, 'merged_from', NEW.merged_from,
                    'metadata_json', NEW.metadata_json, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NEW.normalized_hash);
        END;
        CREATE TRIGGER mc_memories_feed_update AFTER UPDATE ON mc_memories
        WHEN NEW.id IS NOT OLD.id OR NEW.project_path IS NOT OLD.project_path
          OR NEW.category IS NOT OLD.category OR NEW.content IS NOT OLD.content
          OR NEW.normalized_hash IS NOT OLD.normalized_hash OR NEW.importance IS NOT OLD.importance
          OR NEW.scope IS NOT OLD.scope OR NEW.shareable IS NOT OLD.shareable
          OR NEW.source_session_id IS NOT OLD.source_session_id OR NEW.source_type IS NOT OLD.source_type
          OR NEW.seen_count IS NOT OLD.seen_count OR NEW.retrieval_count IS NOT OLD.retrieval_count
          OR NEW.first_seen_at IS NOT OLD.first_seen_at OR NEW.created_at IS NOT OLD.created_at
          OR NEW.updated_at IS NOT OLD.updated_at OR NEW.last_seen_at IS NOT OLD.last_seen_at
          OR NEW.last_retrieved_at IS NOT OLD.last_retrieved_at OR NEW.status IS NOT OLD.status
          OR NEW.expires_at IS NOT OLD.expires_at OR NEW.verification_status IS NOT OLD.verification_status
          OR NEW.verified_at IS NOT OLD.verified_at OR NEW.classified_at IS NOT OLD.classified_at
          OR NEW.superseded_by_memory_id IS NOT OLD.superseded_by_memory_id
          OR NEW.merged_from IS NOT OLD.merged_from OR NEW.metadata_json IS NOT OLD.metadata_json
          OR NEW.context_store_uuid IS NOT OLD.context_store_uuid
          OR NEW.context_row_id IS NOT OLD.context_row_id
        BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('memories', 'update', NEW.id,
                json_object(
                    'id', NEW.id, 'project_path', NEW.project_path, 'category', NEW.category,
                    'content', NEW.content, 'normalized_hash', NEW.normalized_hash,
                    'importance', NEW.importance, 'scope', NEW.scope, 'shareable', NEW.shareable,
                    'source_session_id', NEW.source_session_id, 'source_type', NEW.source_type,
                    'seen_count', NEW.seen_count, 'retrieval_count', NEW.retrieval_count,
                    'first_seen_at', NEW.first_seen_at, 'created_at', NEW.created_at,
                    'updated_at', NEW.updated_at, 'last_seen_at', NEW.last_seen_at,
                    'last_retrieved_at', NEW.last_retrieved_at, 'status', NEW.status,
                    'expires_at', NEW.expires_at, 'verification_status', NEW.verification_status,
                    'verified_at', NEW.verified_at, 'classified_at', NEW.classified_at,
                    'superseded_by_memory_id', NEW.superseded_by_memory_id, 'merged_from', NEW.merged_from,
                    'metadata_json', NEW.metadata_json, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NEW.normalized_hash);
        END;
        CREATE TRIGGER mc_memories_feed_delete AFTER DELETE ON mc_memories BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('memories', 'tombstone', OLD.id,
                json_object(
                    'id', OLD.id, 'project_path', OLD.project_path, 'category', OLD.category,
                    'content', OLD.content, 'normalized_hash', OLD.normalized_hash,
                    'importance', OLD.importance, 'scope', OLD.scope, 'shareable', OLD.shareable,
                    'source_session_id', OLD.source_session_id, 'source_type', OLD.source_type,
                    'seen_count', OLD.seen_count, 'retrieval_count', OLD.retrieval_count,
                    'first_seen_at', OLD.first_seen_at, 'created_at', OLD.created_at,
                    'updated_at', OLD.updated_at, 'last_seen_at', OLD.last_seen_at,
                    'last_retrieved_at', OLD.last_retrieved_at, 'status', OLD.status,
                    'expires_at', OLD.expires_at, 'verification_status', OLD.verification_status,
                    'verified_at', OLD.verified_at, 'classified_at', OLD.classified_at,
                    'superseded_by_memory_id', OLD.superseded_by_memory_id, 'merged_from', OLD.merged_from,
                    'metadata_json', OLD.metadata_json, 'context_store_uuid', OLD.context_store_uuid,
                    'context_row_id', OLD.context_row_id), OLD.normalized_hash);
        END;

        DROP TRIGGER IF EXISTS mc_notes_feed_insert;
        DROP TRIGGER IF EXISTS mc_notes_feed_update;
        DROP TRIGGER IF EXISTS mc_notes_feed_delete;
        CREATE TRIGGER mc_notes_feed_insert AFTER INSERT ON mc_notes BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'insert', NEW.id,
                json_object(
                    'id', NEW.id, 'project_path', NEW.project_path, 'session_id', NEW.session_id,
                    'content', NEW.content, 'status', NEW.status, 'surface_condition', NEW.surface_condition,
                    'anchor_block_id', NEW.anchor_block_id, 'created_at_ms', NEW.created_at_ms,
                    'updated_at_ms', NEW.updated_at_ms, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NULL);
        END;
        CREATE TRIGGER mc_notes_feed_update AFTER UPDATE ON mc_notes
        WHEN NEW.id IS NOT OLD.id OR NEW.project_path IS NOT OLD.project_path
          OR NEW.session_id IS NOT OLD.session_id OR NEW.content IS NOT OLD.content
          OR NEW.status IS NOT OLD.status OR NEW.surface_condition IS NOT OLD.surface_condition
          OR NEW.anchor_block_id IS NOT OLD.anchor_block_id OR NEW.created_at_ms IS NOT OLD.created_at_ms
          OR NEW.updated_at_ms IS NOT OLD.updated_at_ms
          OR NEW.context_store_uuid IS NOT OLD.context_store_uuid
          OR NEW.context_row_id IS NOT OLD.context_row_id
        BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'update', NEW.id,
                json_object(
                    'id', NEW.id, 'project_path', NEW.project_path, 'session_id', NEW.session_id,
                    'content', NEW.content, 'status', NEW.status, 'surface_condition', NEW.surface_condition,
                    'anchor_block_id', NEW.anchor_block_id, 'created_at_ms', NEW.created_at_ms,
                    'updated_at_ms', NEW.updated_at_ms, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NULL);
        END;
        CREATE TRIGGER mc_notes_feed_delete AFTER DELETE ON mc_notes BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'tombstone', OLD.id,
                json_object(
                    'id', OLD.id, 'project_path', OLD.project_path, 'session_id', OLD.session_id,
                    'content', OLD.content, 'status', OLD.status, 'surface_condition', OLD.surface_condition,
                    'anchor_block_id', OLD.anchor_block_id, 'created_at_ms', OLD.created_at_ms,
                    'updated_at_ms', OLD.updated_at_ms, 'context_store_uuid', OLD.context_store_uuid,
                    'context_row_id', OLD.context_row_id), NULL);
        END;
    "#,
    },
    Migration {
        version: 24,
        // Zero-target ctx_reduce commands must still record a ledger row for idempotency
        // (a retry of the same command_id must dedupe), but they never produce pending_agent_drops
        // rows. Without a terminal disposition those rows sit forever with first_applied_at_ms=NULL,
        // making telemetry lie about pending counts. The 'no_targets' disposition marks them as
        // resolved at insert time so they are excluded from "pending" interpretations.
        statements: "
        ALTER TABLE mc_reduce_command_ledger
            ADD COLUMN disposition TEXT
            CHECK (disposition IS NULL OR disposition IN ('no_targets'));
        ",
    },
    Migration {
        version: 25,
        // A foreign-memory revocation changes the workspace cache identity exactly once.
        // The trigger evaluates the complete old/new visibility transition in SQLite so
        // direct SQL and facade mutations cannot bypass the epoch.
        statements: r#"
        CREATE TABLE IF NOT EXISTS mc_memory_visibility_epoch (
            project_path TEXT PRIMARY KEY,
            epoch INTEGER NOT NULL DEFAULT 0
        );
        DROP TRIGGER IF EXISTS mc_memories_visibility_epoch;
        CREATE TRIGGER mc_memories_visibility_epoch
        AFTER UPDATE OF status, expires_at, scope, shareable, category ON mc_memories
        WHEN EXISTS (
            SELECT 1
              FROM mc_workspace_members old_member
              JOIN mc_workspaces old_workspace ON old_workspace.id = old_member.workspace_id
             WHERE old_member.project_path = OLD.project_path
               AND OLD.status IN ('active','permanent')
               AND OLD.shareable = 1
               AND OLD.scope IN ('project','ecosystem','universe')
               AND OLD.category IN (SELECT value FROM json_each(old_workspace.share_categories))
        )
        AND NOT EXISTS (
            SELECT 1
              FROM mc_workspace_members new_member
              JOIN mc_workspaces new_workspace ON new_workspace.id = new_member.workspace_id
             WHERE new_member.project_path = NEW.project_path
               AND NEW.status IN ('active','permanent')
               AND NEW.shareable = 1
               AND NEW.scope IN ('project','ecosystem','universe')
               AND NEW.category IN (SELECT value FROM json_each(new_workspace.share_categories))
        )
        BEGIN
            INSERT INTO mc_memory_visibility_epoch(project_path, epoch)
            VALUES (OLD.project_path, 1)
            ON CONFLICT(project_path) DO UPDATE SET epoch = epoch + 1;
        END;
    "#,
    },
    Migration {
        version: 26,
        // Project-owned notes carry the complete smart-note state machine. Columns carried
        // over from the previous context-note schema are copied as-is, while status_version
        // and delivery rows make every evaluator, surfacer, and mirror transition
        // compare-and-swap safe.
        statements: r#"
        DROP TRIGGER IF EXISTS mc_notes_feed_insert;
        DROP TRIGGER IF EXISTS mc_notes_feed_update;
        DROP TRIGGER IF EXISTS mc_notes_feed_delete;
        DROP TRIGGER IF EXISTS mc_notes_ownership_insert;
        DROP TRIGGER IF EXISTS mc_notes_ownership_update;
        DROP TRIGGER IF EXISTS mc_notes_ownership_delete;
        ALTER TABLE mc_notes RENAME TO mc_notes_v23;
        CREATE TABLE mc_notes (
            id                         INTEGER PRIMARY KEY AUTOINCREMENT,
            type                       TEXT NOT NULL DEFAULT 'smart'
                CHECK (type IN ('session', 'smart')),
            project_path               TEXT NOT NULL,
            session_id                 TEXT,
            content                    TEXT NOT NULL,
            status                     TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'pending', 'ready', 'surfacing', 'surfaced', 'dismissed')),
            surface_condition          TEXT,
            ready_at                   INTEGER,
            ready_reason               TEXT,
            manifest_json              TEXT,
            compiled_check             TEXT,
            check_hash                 TEXT,
            check_cron                 TEXT,
            check_failure_count       INTEGER NOT NULL DEFAULT 0,
            check_network_failure_count INTEGER NOT NULL DEFAULT 0,
            check_quarantined_until   INTEGER,
            check_next_due_at         INTEGER,
            check_compiled_at         INTEGER,
            check_false_since_at      INTEGER,
            check_last_liveness_at    INTEGER,
            last_checked_at           INTEGER,
            check_status               TEXT NOT NULL DEFAULT 'uncompiled',
            check_version              INTEGER NOT NULL DEFAULT 0,
            policy_version            INTEGER NOT NULL DEFAULT 1,
            harness                    TEXT NOT NULL DEFAULT 'module',
            anchor_block_id            TEXT,
            anchor_ordinal             INTEGER,
            dismissed_at              INTEGER,
            dismissal_resolution       TEXT,
            status_version             INTEGER NOT NULL DEFAULT 0,
            created_at_ms             INTEGER NOT NULL DEFAULT 0,
            updated_at_ms             INTEGER NOT NULL DEFAULT 0,
            context_store_uuid        TEXT,
            context_row_id            INTEGER,
            UNIQUE(context_store_uuid, context_row_id)
        );
        INSERT INTO mc_notes (
            id, type, project_path, session_id, content, status, surface_condition,
            anchor_block_id, created_at_ms, updated_at_ms
        )
        SELECT id, 'smart', project_path, session_id, content,
               CASE status WHEN 'active' THEN 'active' ELSE 'dismissed' END,
               surface_condition, anchor_block_id, created_at_ms, updated_at_ms
          FROM mc_notes_v23;
        DROP TABLE mc_notes_v23;
        CREATE INDEX idx_mc_notes_scope_status
            ON mc_notes(project_path, session_id, status, updated_at_ms DESC, id DESC);
        CREATE INDEX idx_mc_notes_due
            ON mc_notes(project_path, status, check_next_due_at, id);

        CREATE TABLE IF NOT EXISTS mc_note_deliveries (
            delivery_id                 TEXT PRIMARY KEY,
            note_id                     INTEGER NOT NULL,
            session_id                  TEXT NOT NULL,
            delivered_pass_fingerprint  TEXT NOT NULL,
            transform_pass_id           TEXT NOT NULL DEFAULT '',
            acked_at                    INTEGER,
            created_at_ms               INTEGER NOT NULL DEFAULT 0,
            UNIQUE(note_id, session_id, delivered_pass_fingerprint)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_note_deliveries_retry
            ON mc_note_deliveries(session_id, acked_at, created_at_ms, note_id);

        -- The module store is protected by a single-writer lease; no external process may
        -- write this database, so every writer is registered with this connection's UDF.
        CREATE TRIGGER mc_notes_ownership_insert
        BEFORE INSERT ON mc_notes
        WHEN NEW.project_path = '' OR mc_note_caller_project() IS NOT NEW.project_path
        BEGIN
            SELECT RAISE(ABORT, 'note ownership insert is outside the caller project');
        END;
        CREATE TRIGGER mc_notes_ownership_update
        BEFORE UPDATE ON mc_notes
        WHEN (NEW.id IS NOT OLD.id OR NEW.type IS NOT OLD.type
              OR NEW.session_id IS NOT OLD.session_id OR NEW.project_path IS NOT OLD.project_path
              OR NEW.context_store_uuid IS NOT OLD.context_store_uuid
              OR NEW.context_row_id IS NOT OLD.context_row_id)
          AND NOT (mc_note_caller_project() IS OLD.project_path
                   OR mc_note_caller_project() IS NEW.project_path)
        BEGIN
            SELECT RAISE(ABORT, 'note ownership update is outside the old or new project');
        END;
        CREATE TRIGGER mc_notes_ownership_delete
        BEFORE DELETE ON mc_notes
        WHEN mc_note_caller_project() IS NOT OLD.project_path
        BEGIN
            SELECT RAISE(ABORT, 'note ownership delete is outside the row project');
        END;

        CREATE TRIGGER mc_notes_feed_insert AFTER INSERT ON mc_notes BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'insert', NEW.id,
                json_object(
                    'id', NEW.id, 'type', NEW.type, 'project_path', NEW.project_path,
                    'session_id', NEW.session_id, 'content', NEW.content, 'status', NEW.status,
                    'surface_condition', NEW.surface_condition, 'ready_at', NEW.ready_at,
                    'ready_reason', NEW.ready_reason, 'manifest_json', NEW.manifest_json,
                    'compiled_check', NEW.compiled_check, 'check_hash', NEW.check_hash,
                    'check_cron', NEW.check_cron, 'check_failure_count', NEW.check_failure_count,
                    'check_network_failure_count', NEW.check_network_failure_count,
                    'check_quarantined_until', NEW.check_quarantined_until,
                    'check_next_due_at', NEW.check_next_due_at, 'check_compiled_at', NEW.check_compiled_at,
                    'check_false_since_at', NEW.check_false_since_at,
                    'check_last_liveness_at', NEW.check_last_liveness_at,
                    'last_checked_at', NEW.last_checked_at, 'check_status', NEW.check_status,
                    'check_version', NEW.check_version, 'policy_version', NEW.policy_version,
                    'harness', NEW.harness, 'anchor_block_id', NEW.anchor_block_id,
                    'anchor_ordinal', NEW.anchor_ordinal, 'dismissed_at', NEW.dismissed_at,
                    'dismissal_resolution', NEW.dismissal_resolution,
                    'status_version', NEW.status_version, 'created_at_ms', NEW.created_at_ms,
                    'updated_at_ms', NEW.updated_at_ms, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NULL);
        END;
        CREATE TRIGGER mc_notes_feed_update AFTER UPDATE ON mc_notes
        WHEN NEW.id IS NOT OLD.id OR NEW.type IS NOT OLD.type
          OR NEW.project_path IS NOT OLD.project_path OR NEW.session_id IS NOT OLD.session_id
          OR NEW.content IS NOT OLD.content OR NEW.status IS NOT OLD.status
          OR NEW.surface_condition IS NOT OLD.surface_condition OR NEW.ready_at IS NOT OLD.ready_at
          OR NEW.ready_reason IS NOT OLD.ready_reason OR NEW.manifest_json IS NOT OLD.manifest_json
          OR NEW.compiled_check IS NOT OLD.compiled_check OR NEW.check_hash IS NOT OLD.check_hash
          OR NEW.check_cron IS NOT OLD.check_cron
          OR NEW.check_failure_count IS NOT OLD.check_failure_count
          OR NEW.check_network_failure_count IS NOT OLD.check_network_failure_count
          OR NEW.check_quarantined_until IS NOT OLD.check_quarantined_until
          OR NEW.check_next_due_at IS NOT OLD.check_next_due_at
          OR NEW.check_compiled_at IS NOT OLD.check_compiled_at
          OR NEW.check_false_since_at IS NOT OLD.check_false_since_at
          OR NEW.check_last_liveness_at IS NOT OLD.check_last_liveness_at
          OR NEW.last_checked_at IS NOT OLD.last_checked_at OR NEW.check_status IS NOT OLD.check_status
          OR NEW.check_version IS NOT OLD.check_version OR NEW.policy_version IS NOT OLD.policy_version
          OR NEW.harness IS NOT OLD.harness OR NEW.anchor_block_id IS NOT OLD.anchor_block_id
          OR NEW.anchor_ordinal IS NOT OLD.anchor_ordinal OR NEW.dismissed_at IS NOT OLD.dismissed_at
          OR NEW.dismissal_resolution IS NOT OLD.dismissal_resolution
          OR NEW.status_version IS NOT OLD.status_version
          OR NEW.created_at_ms IS NOT OLD.created_at_ms OR NEW.updated_at_ms IS NOT OLD.updated_at_ms
          OR NEW.context_store_uuid IS NOT OLD.context_store_uuid
          OR NEW.context_row_id IS NOT OLD.context_row_id
        BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'update', NEW.id,
                json_object(
                    'id', NEW.id, 'type', NEW.type, 'project_path', NEW.project_path,
                    'session_id', NEW.session_id, 'content', NEW.content, 'status', NEW.status,
                    'surface_condition', NEW.surface_condition, 'ready_at', NEW.ready_at,
                    'ready_reason', NEW.ready_reason, 'manifest_json', NEW.manifest_json,
                    'compiled_check', NEW.compiled_check, 'check_hash', NEW.check_hash,
                    'check_cron', NEW.check_cron, 'check_failure_count', NEW.check_failure_count,
                    'check_network_failure_count', NEW.check_network_failure_count,
                    'check_quarantined_until', NEW.check_quarantined_until,
                    'check_next_due_at', NEW.check_next_due_at, 'check_compiled_at', NEW.check_compiled_at,
                    'check_false_since_at', NEW.check_false_since_at,
                    'check_last_liveness_at', NEW.check_last_liveness_at,
                    'last_checked_at', NEW.last_checked_at, 'check_status', NEW.check_status,
                    'check_version', NEW.check_version, 'policy_version', NEW.policy_version,
                    'harness', NEW.harness, 'anchor_block_id', NEW.anchor_block_id,
                    'anchor_ordinal', NEW.anchor_ordinal, 'dismissed_at', NEW.dismissed_at,
                    'dismissal_resolution', NEW.dismissal_resolution,
                    'status_version', NEW.status_version, 'created_at_ms', NEW.created_at_ms,
                    'updated_at_ms', NEW.updated_at_ms, 'context_store_uuid', NEW.context_store_uuid,
                    'context_row_id', NEW.context_row_id), NULL);
        END;
        CREATE TRIGGER mc_notes_feed_delete AFTER DELETE ON mc_notes BEGIN
            INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash)
            VALUES ('notes', 'tombstone', OLD.id,
                json_object(
                    'id', OLD.id, 'type', OLD.type, 'project_path', OLD.project_path,
                    'session_id', OLD.session_id, 'content', OLD.content, 'status', OLD.status,
                    'surface_condition', OLD.surface_condition, 'ready_at', OLD.ready_at,
                    'ready_reason', OLD.ready_reason, 'manifest_json', OLD.manifest_json,
                    'compiled_check', OLD.compiled_check, 'check_hash', OLD.check_hash,
                    'check_cron', OLD.check_cron, 'check_failure_count', OLD.check_failure_count,
                    'check_network_failure_count', OLD.check_network_failure_count,
                    'check_quarantined_until', OLD.check_quarantined_until,
                    'check_next_due_at', OLD.check_next_due_at, 'check_compiled_at', OLD.check_compiled_at,
                    'check_false_since_at', OLD.check_false_since_at,
                    'check_last_liveness_at', OLD.check_last_liveness_at,
                    'last_checked_at', OLD.last_checked_at, 'check_status', OLD.check_status,
                    'check_version', OLD.check_version, 'policy_version', OLD.policy_version,
                    'harness', OLD.harness, 'anchor_block_id', OLD.anchor_block_id,
                    'anchor_ordinal', OLD.anchor_ordinal, 'dismissed_at', OLD.dismissed_at,
                    'dismissal_resolution', OLD.dismissal_resolution,
                    'status_version', OLD.status_version, 'created_at_ms', OLD.created_at_ms,
                    'updated_at_ms', OLD.updated_at_ms, 'context_store_uuid', OLD.context_store_uuid,
                    'context_row_id', OLD.context_row_id), NULL);
        END;
    "#,
    },
    Migration {
        version: 27,
        // Store each accepted seed row unchanged so the module can compute and verify its own
        // digest. Mark every delivery attempt as terminal, and recreate the visibility trigger
        // so expired rows follow the same eligibility rules as other invisible rows.
        statements: r#"
        CREATE TABLE IF NOT EXISTS mc_authority_seed_rows (
            context_store_uuid TEXT NOT NULL,
            project TEXT NOT NULL,
            domain TEXT NOT NULL CHECK(domain IN ('memories','notes')),
            source_row_id INTEGER NOT NULL,
            snapshot_json TEXT NOT NULL,
            PRIMARY KEY(context_store_uuid, project, domain, source_row_id)
        );
        CREATE TABLE IF NOT EXISTS mc_authority_pending_memory_references (
            context_store_uuid TEXT NOT NULL,
            source_context_row_id INTEGER NOT NULL,
            target_context_row_id INTEGER NOT NULL,
            PRIMARY KEY(context_store_uuid, source_context_row_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_authority_pending_memory_reference_target
            ON mc_authority_pending_memory_references(context_store_uuid, target_context_row_id);

        ALTER TABLE mc_note_deliveries ADD COLUMN project_path TEXT NOT NULL DEFAULT '';
        ALTER TABLE mc_note_deliveries ADD COLUMN disposition TEXT
            CHECK(disposition IS NULL OR disposition IN ('acked','nacked','superseded'));
        UPDATE mc_note_deliveries
           SET project_path = COALESCE((SELECT project_path FROM mc_notes WHERE id = note_id), ''),
               disposition = CASE WHEN acked_at IS NOT NULL THEN 'acked' ELSE NULL END;
        DROP INDEX IF EXISTS idx_mc_note_deliveries_retry;
        CREATE INDEX idx_mc_note_deliveries_retry
            ON mc_note_deliveries(project_path, session_id, disposition, created_at_ms, note_id);

        DROP TRIGGER IF EXISTS mc_memories_visibility_epoch;
        CREATE TRIGGER mc_memories_visibility_epoch
        AFTER UPDATE OF status, expires_at, scope, shareable, category ON mc_memories
        WHEN EXISTS (
            SELECT 1
              FROM mc_workspace_members old_member
              JOIN mc_workspaces old_workspace ON old_workspace.id = old_member.workspace_id
             WHERE old_member.project_path = OLD.project_path
               AND OLD.status IN ('active','permanent')
               AND OLD.shareable = 1
               AND OLD.scope IN ('project','ecosystem','universe')
               AND OLD.category IN (SELECT value FROM json_each(old_workspace.share_categories))
        )
        OR EXISTS (
            SELECT 1
              FROM mc_workspace_members new_member
              JOIN mc_workspaces new_workspace ON new_workspace.id = new_member.workspace_id
             WHERE new_member.project_path = NEW.project_path
               AND NEW.status IN ('active','permanent')
               AND NEW.shareable = 1
               AND NEW.scope IN ('project','ecosystem','universe')
               AND NEW.category IN (SELECT value FROM json_each(new_workspace.share_categories))
        )
        BEGIN
            -- Ignore expiry in the trigger clock: SQLite's second-resolution now can
            -- disagree with the caller's now_ms and miss same-second revocations.
            -- A rare extra baseline recompute is acceptable; a missed revocation is not.
            INSERT INTO mc_memory_visibility_epoch(project_path, epoch)
            VALUES (OLD.project_path, 1)
            ON CONFLICT(project_path) DO UPDATE SET epoch = epoch + 1;
        END;
        "#,
    },
    Migration {
        version: 28,
        // Scope pending seed references per project/domain, mint drain coordinator
        // tokens, and keep the visibility trigger free of SQLite clock comparisons.
        statements: r#"
        CREATE TABLE mc_authority_pending_memory_references_v28 (
            context_store_uuid TEXT NOT NULL,
            project TEXT NOT NULL,
            domain TEXT NOT NULL CHECK(domain IN ('memories','notes')),
            source_context_row_id INTEGER NOT NULL,
            target_context_row_id INTEGER NOT NULL,
            PRIMARY KEY(context_store_uuid, project, domain, source_context_row_id)
        );
        INSERT INTO mc_authority_pending_memory_references_v28(
            context_store_uuid, project, domain, source_context_row_id, target_context_row_id
        )
        SELECT p.context_store_uuid,
               COALESCE(
                   (SELECT m.project_path FROM mc_memories m
                     WHERE m.context_store_uuid = p.context_store_uuid
                       AND m.context_row_id = p.source_context_row_id),
                   ''
               ),
               'memories',
               p.source_context_row_id,
               p.target_context_row_id
          FROM mc_authority_pending_memory_references p;
        -- A pending row whose source memory no longer exists has no project to scope
        -- it under; every runtime query requires a concrete project, so keeping the
        -- row would only strand it forever. Dropping it is safe: the reference is
        -- re-derived from wire rows on the next seed of whichever project owns it.
        DELETE FROM mc_authority_pending_memory_references_v28 WHERE project = '';
        DROP TABLE mc_authority_pending_memory_references;
        ALTER TABLE mc_authority_pending_memory_references_v28
            RENAME TO mc_authority_pending_memory_references;
        CREATE INDEX IF NOT EXISTS idx_mc_authority_pending_memory_reference_target
            ON mc_authority_pending_memory_references(
                context_store_uuid, project, domain, target_context_row_id
            );

        ALTER TABLE mc_authority ADD COLUMN coordinator_token TEXT;

        DROP TRIGGER IF EXISTS mc_memories_visibility_epoch;
        CREATE TRIGGER mc_memories_visibility_epoch
        AFTER UPDATE OF status, expires_at, scope, shareable, category ON mc_memories
        WHEN EXISTS (
            SELECT 1
              FROM mc_workspace_members old_member
              JOIN mc_workspaces old_workspace ON old_workspace.id = old_member.workspace_id
             WHERE old_member.project_path = OLD.project_path
               AND OLD.status IN ('active','permanent')
               AND OLD.shareable = 1
               AND OLD.scope IN ('project','ecosystem','universe')
               AND OLD.category IN (SELECT value FROM json_each(old_workspace.share_categories))
        )
        OR EXISTS (
            SELECT 1
              FROM mc_workspace_members new_member
              JOIN mc_workspaces new_workspace ON new_workspace.id = new_member.workspace_id
             WHERE new_member.project_path = NEW.project_path
               AND NEW.status IN ('active','permanent')
               AND NEW.shareable = 1
               AND NEW.scope IN ('project','ecosystem','universe')
               AND NEW.category IN (SELECT value FROM json_each(new_workspace.share_categories))
        )
        BEGIN
            -- Ignore expiry in the trigger clock: SQLite's second-resolution now can
            -- disagree with the caller's now_ms and miss same-second revocations.
            -- A rare extra baseline recompute is acceptable; a missed revocation is not.
            INSERT INTO mc_memory_visibility_epoch(project_path, epoch)
            VALUES (OLD.project_path, 1)
            ON CONFLICT(project_path) DO UPDATE SET epoch = epoch + 1;
        END;
        "#,
    },
    Migration {
        version: 29,
        // The module store uses project identity as its domain key. A route root is
        // transport vocabulary, so retain the mapping only to normalize legacy facade
        // rows when a MODULE authority route is observed.
        statements: r#"
        CREATE TABLE IF NOT EXISTS mc_authority_route_bindings (
            route_project_root TEXT PRIMARY KEY,
            context_store_uuid TEXT NOT NULL,
            project            TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mc_authority_route_bindings_authority
            ON mc_authority_route_bindings(context_store_uuid, project);

        CREATE TRIGGER mc_authority_route_binding_rekey_insert
        AFTER INSERT ON mc_authority_route_bindings
        WHEN NEW.route_project_root LIKE '/%'
        BEGIN
            DELETE FROM mc_memories
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               )
               AND EXISTS (
                    SELECT 1 FROM mc_memories canonical
                     WHERE canonical.project_path = NEW.project
                       AND canonical.category = mc_memories.category
                       AND canonical.normalized_hash = mc_memories.normalized_hash
               );
            UPDATE mc_memories
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               );
            UPDATE mc_memory_mutation_log
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               );
            UPDATE mc_notes
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'notes'
                       AND authority.state = 'MODULE'
               );
        END;

        CREATE TRIGGER mc_authority_route_binding_rekey_update
        AFTER UPDATE OF context_store_uuid, project ON mc_authority_route_bindings
        WHEN NEW.route_project_root LIKE '/%'
        BEGIN
            DELETE FROM mc_memories
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               )
               AND EXISTS (
                    SELECT 1 FROM mc_memories canonical
                     WHERE canonical.project_path = NEW.project
                       AND canonical.category = mc_memories.category
                       AND canonical.normalized_hash = mc_memories.normalized_hash
               );
            UPDATE mc_memories
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               );
            UPDATE mc_memory_mutation_log
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'memories'
                       AND authority.state = 'MODULE'
               );
            UPDATE mc_notes
               SET project_path = NEW.project
             WHERE project_path = NEW.route_project_root
               AND EXISTS (
                    SELECT 1 FROM mc_authority authority
                     WHERE authority.context_store_uuid = NEW.context_store_uuid
                       AND authority.project = NEW.project
                       AND authority.domain = 'notes'
                       AND authority.state = 'MODULE'
               );
        END;
        "#,
    },
    Migration {
        version: 30,
        // Migration history is immutable: live stores already recorded this idempotent route
        // normalization step as version 30, so fresh stores must record the same version.
        statements: r#"
        DELETE FROM mc_memories
         WHERE EXISTS (
                SELECT 1
                  FROM mc_authority_route_bindings binding
                  JOIN mc_authority authority
                    ON authority.context_store_uuid = binding.context_store_uuid
                   AND authority.project = binding.project
                   AND authority.domain = 'memories'
                   AND authority.state = 'MODULE'
                 WHERE binding.route_project_root = mc_memories.project_path
               )
           AND EXISTS (
                SELECT 1
                  FROM mc_authority_route_bindings binding
                  JOIN mc_authority authority
                    ON authority.context_store_uuid = binding.context_store_uuid
                   AND authority.project = binding.project
                   AND authority.domain = 'memories'
                   AND authority.state = 'MODULE'
                  JOIN mc_memories canonical
                    ON canonical.project_path = binding.project
                   AND canonical.category = mc_memories.category
                   AND canonical.normalized_hash = mc_memories.normalized_hash
                 WHERE binding.route_project_root = mc_memories.project_path
               );
        UPDATE mc_memories
           SET project_path = (
               SELECT binding.project
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memories.project_path
           )
         WHERE EXISTS (
               SELECT 1
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memories.project_path
           );
        UPDATE mc_memory_mutation_log
           SET project_path = (
               SELECT binding.project
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memory_mutation_log.project_path
           )
         WHERE EXISTS (
               SELECT 1
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memory_mutation_log.project_path
           );
        "#,
    },
    Migration {
        version: 31,
        // Classify retries must replay the recorded module outcome instead of sending the
        // memory pool to a provider twice. This ledger is separate from cache revisions because
        // classification changes only metadata and must not create a new cache revision.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_dream_task_commands (
            session_id   TEXT NOT NULL,
            command_id   TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            PRIMARY KEY (session_id, command_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_dream_task_commands_created
            ON mc_dream_task_commands(session_id, created_at, command_id);
    ",
    },
    Migration {
        version: 32,
        // Bindings may have been stored before the route became authority-owned or before
        // facade rows were written. Normalize every stored binding during upgrade by removing
        // duplicate memory rows and rekeying the remaining route, mutation, and note rows,
        // so cleanup does not depend on trigger timing or a later status request.
        statements: r#"
        DELETE FROM mc_memories
         WHERE EXISTS (
                SELECT 1
                  FROM mc_authority_route_bindings binding
                  JOIN mc_authority authority
                    ON authority.context_store_uuid = binding.context_store_uuid
                   AND authority.project = binding.project
                   AND authority.domain = 'memories'
                   AND authority.state = 'MODULE'
                 WHERE binding.route_project_root = mc_memories.project_path
               )
           AND EXISTS (
                SELECT 1
                  FROM mc_authority_route_bindings binding
                  JOIN mc_authority authority
                    ON authority.context_store_uuid = binding.context_store_uuid
                   AND authority.project = binding.project
                   AND authority.domain = 'memories'
                   AND authority.state = 'MODULE'
                  JOIN mc_memories canonical
                    ON canonical.project_path = binding.project
                   AND canonical.category = mc_memories.category
                   AND canonical.normalized_hash = mc_memories.normalized_hash
                 WHERE binding.route_project_root = mc_memories.project_path
               );
        UPDATE mc_memories
           SET project_path = (
               SELECT binding.project
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memories.project_path
           )
         WHERE EXISTS (
               SELECT 1
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memories.project_path
           );
        UPDATE mc_memory_mutation_log
           SET project_path = (
               SELECT binding.project
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memory_mutation_log.project_path
           )
         WHERE EXISTS (
               SELECT 1
                 FROM mc_authority_route_bindings binding
                 JOIN mc_authority authority
                   ON authority.context_store_uuid = binding.context_store_uuid
                  AND authority.project = binding.project
                  AND authority.domain = 'memories'
                  AND authority.state = 'MODULE'
                WHERE binding.route_project_root = mc_memory_mutation_log.project_path
           );
        "#,
    },
    Migration {
        version: 33,
        // Persist the authenticated session/root pair with cache state, and make the module
        // writer transaction itself reject facade mutations after authority starts draining.
        statements: r#"
        CREATE TABLE IF NOT EXISTS mc_transform_session_roots (
            session_id  TEXT NOT NULL,
            project_root TEXT NOT NULL,
            observed_at INTEGER NOT NULL,
            PRIMARY KEY(session_id, project_root)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_transform_session_roots_observed
            ON mc_transform_session_roots(observed_at);

        CREATE TRIGGER mc_memories_facade_authority_insert
        BEFORE INSERT ON mc_memories
        WHEN mc_facade_authority_domain() = 'memories'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'memories'
               AND authority.project = NEW.project_path
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;
        CREATE TRIGGER mc_memories_facade_authority_update
        BEFORE UPDATE ON mc_memories
        WHEN mc_facade_authority_domain() = 'memories'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'memories'
               AND authority.project IN (OLD.project_path, NEW.project_path)
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;
        CREATE TRIGGER mc_memories_facade_authority_delete
        BEFORE DELETE ON mc_memories
        WHEN mc_facade_authority_domain() = 'memories'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'memories'
               AND authority.project = OLD.project_path
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;

        CREATE TRIGGER mc_notes_facade_authority_insert
        BEFORE INSERT ON mc_notes
        WHEN mc_facade_authority_domain() = 'notes'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'notes'
               AND authority.project = NEW.project_path
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;
        CREATE TRIGGER mc_notes_facade_authority_update
        BEFORE UPDATE ON mc_notes
        WHEN mc_facade_authority_domain() = 'notes'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'notes'
               AND authority.project IN (OLD.project_path, NEW.project_path)
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;
        CREATE TRIGGER mc_notes_facade_authority_delete
        BEFORE DELETE ON mc_notes
        WHEN mc_facade_authority_domain() = 'notes'
          AND EXISTS (
              SELECT 1 FROM mc_authority_route_bindings binding
              JOIN mc_authority authority
                ON authority.context_store_uuid = binding.context_store_uuid
               AND authority.project = binding.project
             WHERE binding.route_project_root = mc_facade_authority_route()
               AND authority.domain = 'notes'
               AND authority.project = OLD.project_path
               AND authority.state != 'MODULE'
          )
        BEGIN SELECT RAISE(ABORT, 'authority_draining'); END;
        "#,
    },
    Migration {
        version: 34,
        // Mappings are part of the persisted memory state, not merely a TypeScript verification cache.
        // Store the complete file set so applying a revision can restore mappings correctly after
        // data is mirrored or drained.
        statements: r#"
        CREATE TABLE IF NOT EXISTS mc_memory_mappings (
            memory_id        INTEGER PRIMARY KEY,
            project_path     TEXT NOT NULL,
            mapped_files_json TEXT NOT NULL,
            updated_at       INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mc_memory_mappings_project
            ON mc_memory_mappings(project_path, memory_id);
        "#,
    },
    Migration {
        version: 35,
        // Cache rows are not deleted when a session is closed. Keep an explicit activity
        // watermark so lineage pruning can distinguish an old idle session from one that
        // still commits state, instead of treating row existence as proof of liveness.
        statements: r#"
        ALTER TABLE mc_cache_state ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;
        UPDATE mc_cache_state
           SET last_activity_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
         WHERE last_activity_at = 0;
        "#,
    },
    Migration {
        version: 36,
        // The module has no direct reader for these TS-owned side channels yet. Keep the
        // module rows structurally compatible for a later mirror; `compartment_id` stores the
        // module's (session_id, sequence) surrogate because mc_compartments has no row id.
        // Clear-session and re-cut operations explicitly remove session-scoped rows.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_compartment_events (
            id                    INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id            TEXT NOT NULL,
            compartment_id        INTEGER,
            at_compartment        INTEGER,
            kind                  TEXT NOT NULL,
            fields_json           TEXT NOT NULL DEFAULT '{}',
            created_at             INTEGER NOT NULL DEFAULT 0,
            harness                TEXT NOT NULL DEFAULT 'module'
        );
        CREATE INDEX IF NOT EXISTS idx_mc_compartment_events_session
            ON mc_compartment_events(session_id, id);

        CREATE TABLE IF NOT EXISTS mc_primer_candidates (
            id                       INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path             TEXT NOT NULL,
            harness                  TEXT NOT NULL DEFAULT 'module',
            session_id               TEXT NOT NULL,
            question                 TEXT NOT NULL,
            normalized_question      TEXT NOT NULL,
            source_compartment_start INTEGER,
            source_compartment_end   INTEGER,
            source_start_message_id  TEXT NOT NULL DEFAULT '',
            source_end_message_id    TEXT NOT NULL DEFAULT '',
            source_message_time      INTEGER NOT NULL DEFAULT 0,
            created_at               INTEGER NOT NULL DEFAULT 0,
            UNIQUE(project_path, harness, session_id, source_start_message_id, source_end_message_id)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_primer_candidates_project
            ON mc_primer_candidates(project_path, created_at, id);

        CREATE TABLE IF NOT EXISTS mc_user_memory_candidates (
            id                       INTEGER PRIMARY KEY AUTOINCREMENT,
            content                  TEXT NOT NULL,
            session_id               TEXT NOT NULL,
            source_compartment_start INTEGER,
            source_compartment_end   INTEGER,
            created_at               INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_mc_user_memory_candidates_session
            ON mc_user_memory_candidates(session_id, created_at, id);
    ",
    },
    Migration {
        version: 37,
        // Visibility corrections are delivered as m1 removed deltas. Removing the old
        // eager epoch trigger keeps expiry and archival changes out of workspace identity.
        statements: "DROP TRIGGER IF EXISTS mc_memories_visibility_epoch;",
    },
];

/// Apply the route-to-identity vocabulary law inside the caller's fenced transaction.
/// Deletes only content twins, then rekeys remaining route rows and their mutation/note
/// companions. The authority predicate is repeated on every statement so a binding is
/// harmless until its domain is actually MODULE-owned.
fn normalize_authority_route_tx(
    tx: &rusqlite::Transaction<'_>,
    context_store_uuid: &str,
    project: &str,
    route_project_root: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM mc_memories
          WHERE project_path = ?3
            AND EXISTS (
                SELECT 1 FROM mc_authority
                 WHERE context_store_uuid = ?1
                   AND project = ?2
                   AND domain = 'memories'
                   AND state = 'MODULE'
            )
            AND EXISTS (
                SELECT 1 FROM mc_memories canonical
                 WHERE canonical.project_path = ?2
                   AND canonical.category = mc_memories.category
                   AND canonical.normalized_hash = mc_memories.normalized_hash
            )",
        params![context_store_uuid, project, route_project_root],
    )?;
    tx.execute(
        "UPDATE mc_memories
            SET project_path = ?2
          WHERE project_path = ?3
            AND EXISTS (
                SELECT 1 FROM mc_authority
                 WHERE context_store_uuid = ?1
                   AND project = ?2
                   AND domain = 'memories'
                   AND state = 'MODULE'
            )",
        params![context_store_uuid, project, route_project_root],
    )?;
    tx.execute(
        "UPDATE mc_memory_mutation_log
            SET project_path = ?2
          WHERE project_path = ?3
            AND EXISTS (
                SELECT 1 FROM mc_authority
                 WHERE context_store_uuid = ?1
                   AND project = ?2
                   AND domain = 'memories'
                   AND state = 'MODULE'
            )",
        params![context_store_uuid, project, route_project_root],
    )?;
    tx.execute(
        "UPDATE mc_notes
            SET project_path = ?2
          WHERE project_path = ?3
            AND EXISTS (
                SELECT 1 FROM mc_authority
                 WHERE context_store_uuid = ?1
                   AND project = ?2
                   AND domain = 'notes'
                   AND state = 'MODULE'
            )",
        params![context_store_uuid, project, route_project_root],
    )?;
    Ok(())
}

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

/// A historian event retained for a future module-to-TS mirror. `at_compartment` keeps the
/// producer's one-based anchor even when the module-side compartment surrogate is unavailable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorianEventCandidate {
    pub kind: String,
    pub at_compartment: Option<u64>,
    pub compartment_id: Option<u64>,
    pub fields_json: String,
    pub created_at: i64,
    pub harness: String,
}

/// A primer candidate records a question that should remain available across sessions, along with
/// the project, session, and source-compartment information needed to trace where it came from.
/// Its fields match the corresponding TypeScript primer record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorianPrimerCandidate {
    pub project_path: String,
    pub session_id: String,
    pub question: String,
    pub source_compartment_start: Option<u64>,
    pub source_compartment_end: Option<u64>,
    pub source_start_message_id: String,
    pub source_end_message_id: String,
    pub source_message_time: i64,
    pub created_at: i64,
}

/// A privacy-gated user observation candidate. It is intentionally not a project memory:
/// the TS review task owns promotion after the user opts into collection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistorianUserMemoryCandidate {
    pub content: String,
    pub session_id: String,
    pub source_compartment_start: Option<u64>,
    pub source_compartment_end: Option<u64>,
    pub created_at: i64,
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
    pub promote_facts: bool,
    pub events: &'a [HistorianEventCandidate],
    pub primer_candidates: &'a [HistorianPrimerCandidate],
    pub user_memory_candidates: &'a [HistorianUserMemoryCandidate],
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
    /// A caller-supplied publication fence refused the publish before any write.
    /// Distinct from CasConflict so callers can abandon the run WITHOUT arming a
    /// model-failure cooldown: a fence rejection is a fast local race, not a
    /// producer failure, and an immediate retry with a fresh snapshot is valid.
    FenceRejected {
        reason: String,
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
            HistorianPublishError::FenceRejected { reason } => {
                write!(f, "publication fence rejected: {reason}")
            }
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteNudgeAnchorSeed {
    pub message_id: String,
    pub text: String,
}

/// The non-CoreState durable blob: bootstrap + epoch-detection + coverage watermark.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModuleMeta {
    /// A baseline has been materialized at least once. Gates the bootstrap-Hard rule.
    pub initialized: bool,
    /// The render-config fingerprint as of the last Hard fold; an incoming pass whose
    /// fingerprint differs is an epoch change → Hard.
    pub last_render_config: String,
    /// Provider/model/system/upgrade identity observations used to adopt legacy rows without
    /// treating the first non-empty observation as a cache eviction.
    #[serde(default)]
    pub last_provider_id: String,
    #[serde(default)]
    pub last_model_key: String,
    #[serde(default)]
    pub last_system_prompt_hash: String,
    #[serde(default)]
    pub last_upgrade_state: String,
    /// The terminal covered ordinal as of the last baseline. Monotonic-absolute,
    /// never positional; can DECREASE on a revert-Hard.
    pub coverage_ordinal: Option<u64>,
    /// Last normalized `todowrite` view captured on a bust pass. This is deliberately
    /// session-scoped: a todo list is the working state of one conversation, not a
    /// project-shared memory or preference.
    #[serde(default)]
    pub last_todo_state: Option<String>,
    /// Message id that owns the last captured todo state. Host-side todo forwarding uses
    /// this to make retries of one tool result harmless without suppressing newer states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_todo_state_owner_message_id: Option<String>,
    /// SHA-256 of the normalized todo state, used with the owner id for replay detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_todo_state_hash: Option<String>,
    /// Set by session.flush and consumed by the next eligible transform as a SOFT refresh.
    #[serde(default)]
    pub soft_refresh_pending: bool,
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
    /// Sticky note-nudge decisions copied from TypeScript during bootstrap. The host
    /// replays these anchors after native serving so a mode transition keeps them visible.
    #[serde(default)]
    pub note_nudge_anchors: Vec<NoteNudgeAnchorSeed>,
    /// The applied in-session revision the frozen m1 block was last rendered from.
    /// A signal mismatch is pending work; it is not itself permission to bust the provider
    /// cache. 0 is retained for pre-materialization metadata.
    #[serde(default)]
    pub m1_revision: u64,
    /// External render lane fingerprint applied by the last HARD fold. Workspace changes
    /// remain eager-HARD and are kept separate from the deferred in-session lane.
    #[serde(default)]
    pub m1_external_revision: u64,
    /// Latest project memory epoch received from TypeScript state-sync. A changed epoch
    /// arms an eager HARD on the next transform instead of silently becoming an m1 delta.
    #[serde(default)]
    pub project_memory_epoch: u64,
    #[serde(default)]
    pub project_memory_epoch_pending: bool,
    /// Latest global user-profile version received from state-sync. It participates in the
    /// in-session m1 signal and therefore defers until a genuine bust opportunity.
    #[serde(default)]
    pub user_profile_version: u64,
    /// Best-effort durable start of the currently pending in-session delta. It is populated
    /// by state-sync watermark edges, not by a pure defer transform, so defer remains a
    /// zero-write replay path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m1_pending_since_ms: Option<i64>,

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
    /// Highest absolute message ordinal whose old OpenCode reasoning was cleared.
    /// The watermark is durable so a defer pass can replay the same empty sentinels
    /// after OpenCode rebuilds the native message array from its database.
    #[serde(default)]
    pub reasoning_cleared_through_ordinal: u64,
    /// The request-local Claude Code mechanics state committed with the rendered identity.
    /// Missing legacy metadata is false, which preserves the dormant render path.
    #[serde(default)]
    pub cc_u1_active: bool,
    /// The request-local tagging surface latch committed with the rendered identity.
    /// Both the current request and this durable latch must be active before overlay bytes
    /// render, so a transition pass can coordinate one cache-breaking HARD first.
    #[serde(default)]
    pub tagging_surface_active: bool,
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
    pub command_id: Option<String>,
    pub command_first_applied_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendOutcome {
    pub queued: u64,
    pub duplicate: bool,
    /// Terminal disposition for the ledger row, set when the command resolved zero targets.
    /// NULL means the command produced pending drops (normal path).
    pub disposition: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalMarkInput {
    pub ordinal: u64,
    pub block_id: String,
    pub marker_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalMarkRow {
    pub block_id: String,
    pub marker_text: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserHintDecisionInput {
    pub ordinal: u64,
    pub block_id: String,
    pub hint_text: String,
}

/// Overlay writes staged by a transform and committed only after every local validation
/// has accepted the pass. The cache-state CAS serializes competing speculative renders.
#[derive(Debug, Default)]
pub struct TransformOverlayBatch<'a> {
    pub max_seen_ordinal: Option<u64>,
    pub tag_mints: &'a [TagMintInput],
    pub temporal_marks: &'a [TemporalMarkInput],
    pub user_hint: Option<&'a UserHintDecisionInput>,
    pub channel1_append: Option<&'a Channel1AppendRow>,
    pub created_at_ms: i64,
}

impl TransformOverlayBatch<'_> {
    pub fn is_empty(&self) -> bool {
        self.max_seen_ordinal.is_none()
            && self.tag_mints.is_empty()
            && self.temporal_marks.is_empty()
            && self.user_hint.is_none()
            && self.channel1_append.is_none()
    }
}

pub struct TransformCommit<'a> {
    pub expected: Option<u64>,
    pub core: &'a CoreState,
    pub meta: &'a ModuleMeta,
    pub consumed_drop_ids: &'a [i64],
    pub first_applied_command_ids: &'a [String],
    pub memory_revision: Option<&'a MemoryRevision>,
    /// Highest compartment sequence observed while composing a bust. The fenced commit
    /// re-reads this scalar so a publication interleaved between signal read and commit
    /// cannot be hidden behind the older rendered m1 bytes.
    pub compartment_max_seq: Option<i64>,
    /// Authenticated filesystem root observed by the transform that owns this cache commit.
    pub project_root: Option<&'a str>,
    pub overlays: TransformOverlayBatch<'a>,
}

#[derive(Debug, Clone)]
pub struct SessionStatusSnapshot {
    pub loaded: LoadedState,
    pub compartment_count: usize,
    pub pending_drop_count: usize,
    pub tag_count: usize,
    pub pass_trace: Option<PassTrace>,
}

/// A bounded chronological page of module-owned compartments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompartmentPage {
    pub compartments: Vec<StoredCompartment>,
    pub max_sequence: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapupCommandRow {
    pub disposition: String,
    pub rounds: usize,
    pub summary: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecompCommandRow {
    pub disposition: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStateSetOutcome {
    Updated { row_version: u64 },
    Noop,
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
    pub context_store_uuid: Option<String>,
    pub context_row_id: Option<i64>,
}

/// Every column that a memories changefeed snapshot must contain. Keep this list aligned with
/// `mc_memories`; the invariant test below queries the schema and checks every emitted snapshot.
pub const MEMORY_FEED_COLUMNS: &[&str] = &[
    "id",
    "project_path",
    "category",
    "content",
    "normalized_hash",
    "importance",
    "scope",
    "shareable",
    "source_session_id",
    "source_type",
    "seen_count",
    "retrieval_count",
    "first_seen_at",
    "created_at",
    "updated_at",
    "last_seen_at",
    "last_retrieved_at",
    "status",
    "expires_at",
    "verification_status",
    "verified_at",
    "classified_at",
    "superseded_by_memory_id",
    "merged_from",
    "metadata_json",
    "context_store_uuid",
    "context_row_id",
];

/// Inputs for an additive ctx_memory write. Duplicate detection follows the plugin's
/// normalized-content hash (`lowercase → collapse whitespace → MD5`): a matching
/// `(project_path, category, normalized_hash)` returns the existing row id instead of
/// inserting a new row.
#[derive(Debug, Clone, Copy)]
pub struct InsertMemoryInput<'a> {
    pub project_path: &'a str,
    /// Bound route root for facade writes. Domain rows use `project_path`; the route
    /// path is only used to enforce the authority vocabulary boundary.
    pub route_project_root: Option<&'a str>,
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
    /// Bound route root for facade writes. The note remains keyed by `project_path`.
    pub route_project_root: Option<&'a str>,
    pub session_id: &'a str,
    pub content: &'a str,
    pub surface_condition: Option<&'a str>,
    pub anchor_block_id: Option<&'a str>,
    pub now_ms: i64,
}

/// Full module-side note write input. `surface_condition` selects the pending smart-note
/// state; an absent condition creates an ordinary active note for legacy callers, while the
/// Rust-mode adapter keeps session-only notes on the TypeScript-owned path.
#[derive(Debug, Clone, Copy)]
pub struct NoteWriteInput<'a> {
    pub project_path: &'a str,
    /// Bound route root for facade writes. The note remains keyed by `project_path`.
    pub route_project_root: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub content: &'a str,
    pub surface_condition: Option<&'a str>,
    pub anchor_block_id: Option<&'a str>,
    pub anchor_ordinal: Option<i64>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNote {
    pub id: i64,
    pub type_name: String,
    pub project_path: String,
    pub session_id: String,
    pub content: String,
    pub status: String,
    pub surface_condition: Option<String>,
    pub ready_at: Option<i64>,
    pub ready_reason: Option<String>,
    pub manifest_json: Option<String>,
    pub compiled_check: Option<String>,
    pub check_hash: Option<String>,
    pub check_cron: Option<String>,
    pub check_failure_count: i64,
    pub check_network_failure_count: i64,
    pub check_quarantined_until: Option<i64>,
    pub check_next_due_at: Option<i64>,
    pub check_compiled_at: Option<i64>,
    pub check_false_since_at: Option<i64>,
    pub check_last_liveness_at: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub check_status: Option<String>,
    pub check_version: Option<i64>,
    pub policy_version: Option<i64>,
    pub harness: String,
    pub anchor_block_id: Option<String>,
    pub anchor_ordinal: Option<i64>,
    pub dismissed_at: Option<i64>,
    pub dismissal_resolution: Option<String>,
    pub status_version: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub context_store_uuid: Option<String>,
    pub context_row_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteDelivery {
    pub delivery_id: String,
    pub note_id: i64,
    pub session_id: String,
    pub delivered_pass_fingerprint: String,
    pub transform_pass_id: String,
    pub acked_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteCasOutcome {
    Applied(StoredNote),
    Conflict { current: Option<StoredNote> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTransitionInput<'a> {
    pub project_path: &'a str,
    pub note_id: i64,
    pub from_status: &'a str,
    pub source_revision: i64,
    pub to_status: &'a str,
    pub result: Option<&'a str>,
    pub now_ms: i64,
}

pub struct NoteEvaluationInput<'a> {
    pub project_path: &'a str,
    pub note_id: i64,
    pub source_revision: i64,
    pub verdict: bool,
    pub compiled_check: Option<&'a str>,
    pub manifest_json: Option<&'a str>,
    pub check_hash: Option<&'a str>,
    pub next_due_at: Option<i64>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNoteSearchRow {
    pub id: i64,
    pub content: String,
    pub status: String,
    pub surface_condition: Option<String>,
    pub updated_at_ms: i64,
}

/// Durable authority state for one context store, project, and owned domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityRow {
    pub context_store_uuid: String,
    pub project: String,
    pub domain: String,
    pub state: String,
    pub generation: u64,
    pub captured_upper_bound: Option<i64>,
    pub drain_generation: Option<u64>,
    pub drain_cursor: i64,
    pub step_seed: bool,
    pub step_memories: bool,
    pub step_notes: bool,
    pub step_compartments: bool,
    pub step_reconcile: bool,
    pub step_verify: bool,
    pub step_flip: bool,
    pub coordinator_lease: Option<String>,
    pub lease_expires_at: Option<i64>,
    /// Attempt-unique token minted at drain begin/takeover. Step and finish require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_token: Option<String>,
    pub checksum_expected: Option<String>,
    pub checksum_actual: Option<String>,
    pub checksum_ok: Option<bool>,
}

/// One append-only row returned by `mirror.pull`. The snapshot is the complete row
/// as serialized by the feed trigger, including nullable columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangefeedRow {
    pub feed_seq: i64,
    pub domain: String,
    pub op: String,
    pub module_row_id: i64,
    pub full_row_snapshot: Value,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangefeedPage {
    pub domain: String,
    pub cursor: i64,
    pub next_cursor: i64,
    pub has_more: bool,
    pub rows: Vec<ChangefeedRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamTaskCommandRow {
    pub response_json: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationUpdate {
    pub memory_id: i64,
    pub content_hash_at_prompt: String,
    pub importance: Option<i32>,
    pub scope: Option<String>,
    pub shareable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationRejected {
    pub memory_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationApplyResult {
    pub accepted: Vec<i64>,
    pub rejected: Vec<ClassificationRejected>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationUpdate {
    pub memory_id: i64,
    pub content_hash_at_prompt: String,
    pub verification_status: String,
    pub updated_content: Option<String>,
    pub archive_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRejected {
    pub memory_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationApplyResult {
    pub accepted: Vec<i64>,
    pub rejected: Vec<VerificationRejected>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingUpdate {
    pub memory_id: i64,
    pub content_hash_at_prompt: String,
    pub mapped_files: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingRejected {
    pub memory_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingApplyResult {
    pub accepted: Vec<i64>,
    pub rejected: Vec<MappingRejected>,
}

/// A context row used by the crash-idempotent authority seed. The JSON payload is
/// intentionally opaque here; the module persists the fields it owns and verifies
/// the host-provided content hash separately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthoritySeedRow {
    pub source_row_id: i64,
    pub snapshot: Value,
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

/// Cache state and every byte-affecting transform overlay from one SQLite snapshot.
#[derive(Debug, Clone)]
pub struct TransformSnapshot {
    pub loaded: LoadedState,
    pub tags: Vec<McTagRow>,
    pub temporal_marks: Vec<TemporalMarkRow>,
    pub user_hints: Vec<UserHintRow>,
    pub channel1_appends: Vec<Channel1AppendRow>,
    pub overlay_frontier: Option<u64>,
}

#[derive(Debug)]
pub struct WrapupCommandRecord<'a> {
    pub session_id: &'a str,
    pub command_id: &'a str,
    pub disposition: &'a str,
    pub rounds: usize,
    pub summary: &'a str,
    pub created_at: i64,
    pub expected_row_version: Option<u64>,
    pub expected_revert_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordWrapupCommandOutcome {
    Recorded(WrapupCommandRow),
    Stale {
        found_row_version: Option<u64>,
        found_revert_epoch: u64,
    },
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowDropSeedRow {
    pub block_id: String,
    pub related_block_ids: Vec<String>,
    pub drop_mode: String,
    pub payload: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingAgentDropSeedRow {
    pub block_id: String,
    pub queued_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserHintSeedRow {
    pub block_id: String,
    pub hint_text: String,
}

pub struct ShadowStateSyncRequest<'a> {
    pub session_id: &'a str,
    pub shadow_project_path: &'a str,
    pub shadow_generation: u64,
    pub expected_shadow_seq: u64,
    /// The producer's current flat compaction boundary. Present only on a full seed.
    pub seed_boundary_id: Option<&'a str>,
    pub drop_seeds: &'a [ShadowDropSeedRow],
    pub drop_seed_skipped: usize,
    pub pending_agent_drops: &'a [PendingAgentDropSeedRow],
    pub pending_agent_drops_skipped: usize,
    pub user_hint_seeds: &'a [UserHintSeedRow],
    pub auto_search_hint_skipped: usize,
    pub note_nudge_anchors: Option<&'a [NoteNudgeAnchorSeed]>,
    pub todo_synthetic_anchor: Option<&'a FrozenSyntheticTodoPair>,
    pub todo_synthetic_anchor_present: bool,
    pub emergency_latches: Option<(f64, bool, u64)>,
    pub compartments: &'a [StoredCompartment],
    pub memories: &'a [ShadowMemoryRow],
    pub memory_mutations: &'a [ShadowMemoryMutationRow],
    pub user_profile: &'a [String],
    pub workspace: Option<&'a ShadowWorkspaceRow>,
    pub last_todo_state: Option<String>,
    pub project_memory_epoch: Option<u64>,
    pub user_profile_version: Option<u64>,
    pub acked_watermarks: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowStateSyncResult {
    pub shadow_generation: u64,
    pub shadow_seq: u64,
    pub row_version: u64,
    /// True when incoming memory sections were skipped because the module owns memories.
    pub memories_skipped: bool,
    /// Number of TS drop rows that could not be materialized into frozen module units.
    pub drop_seeds_skipped: usize,
    pub pending_agent_drops_seeded: usize,
    pub pending_agent_drops_skipped: usize,
    pub user_hint_seeds_seeded: usize,
    pub auto_search_hint_skipped: usize,
    pub note_nudge_anchors_seeded: usize,
    pub todo_synthetic_anchor_seeded: bool,
    pub emergency_latches_seeded: bool,
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
    AuthoritySeqMismatch { expected: u64, found: u64 },
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
    /// An authority transition was requested from the wrong durable state.
    AuthorityStateMismatch {
        expected: String,
        found: String,
    },
    /// A caller used a stale authority generation after another transition committed.
    AuthorityGenerationMismatch {
        expected: u64,
        found: u64,
    },
    /// The module feed advanced after a drain captured its replay bound.
    AuthorityFeedHeadAdvanced {
        captured: i64,
        found: i64,
    },
    Serde(String),
    MemoryDuplicateContent {
        id: i64,
    },
    NoteCasConflict {
        id: i64,
        expected_status: String,
        expected_version: i64,
        found_status: String,
        found_version: i64,
    },
    NoteOwnershipMismatch {
        id: i64,
        project: String,
    },
    /// A facade route bound to an authority-managed identity attempted to write
    /// using filesystem-path transport vocabulary instead of the domain identity.
    FacadeProjectVocabularyMismatch {
        route_project_root: String,
        authority_project: String,
        write_project: String,
        domain: String,
    },
}

impl std::fmt::Display for McStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McStoreError::Store(e) => write!(f, "store: {e}"),
            McStoreError::CasConflict { expected, found } => {
                write!(f, "cas conflict: expected {expected:?}, found {found}")
            }
            McStoreError::AuthorityStateMismatch { expected, found } => {
                write!(
                    f,
                    "authority state mismatch: expected {expected}, found {found}"
                )
            }
            McStoreError::AuthorityGenerationMismatch { expected, found } => {
                write!(
                    f,
                    "authority generation mismatch: expected {expected}, found {found}"
                )
            }
            McStoreError::AuthorityFeedHeadAdvanced { captured, found } => write!(
                f,
                "authority feed head advanced after drain capture: captured {captured}, found {found}"
            ),
            McStoreError::Serde(e) => write!(f, "serde: {e}"),
            McStoreError::MemoryDuplicateContent { id } => {
                write!(f, "memory content already exists as ID {id}")
            }
            McStoreError::NoteCasConflict {
                id,
                expected_status,
                expected_version,
                found_status,
                found_version,
            } => write!(
                f,
                "note {id} CAS conflict: expected {expected_status}@{expected_version}, found {found_status}@{found_version}"
            ),
            McStoreError::NoteOwnershipMismatch { id, project } => {
                write!(f, "note {id} is not owned by project {project}")
            }
            McStoreError::FacadeProjectVocabularyMismatch {
                route_project_root,
                authority_project,
                write_project,
                domain,
            } => write!(
                f,
                "{domain} facade route {route_project_root} is authority-managed as {authority_project}, but the write used {write_project}"
            ),
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
            ShadowStateSyncError::AuthoritySeqMismatch { expected, found } => write!(
                f,
                "authority seq mismatch: expected {expected}, found {found}"
            ),
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

enum ClassificationTxnOutcome {
    Applied(ClassificationApplyResult),
    AuthorityStateMismatch(String),
    AuthorityGenerationMismatch(u64),
}

enum CommitOutcome {
    Committed(u64),
    CasConflict(u64),
}

enum AuthorityFinishDrainOutcome {
    Finished(Box<AuthorityRow>),
    FeedHeadAdvanced { captured: i64, found: i64 },
}

enum PublishTxnOutcome {
    Committed(HistorianPublishResult),
    CasConflict { found: u64, reason: Option<String> },
    StateMismatch(HistorianDurableState),
    InvalidState(String),
    Serde(String),
}

enum AbandonHistorianTxnOutcome {
    Unchanged,
    Committed(u64),
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

const AUTHORITY_SELECT_SQL: &str = "SELECT context_store_uuid, project, domain, state, generation,
    captured_upper_bound, drain_generation, drain_cursor, step_seed, step_memories,
    step_notes, step_compartments, step_reconcile, step_verify, step_flip,
    coordinator_lease, lease_expires_at, coordinator_token,
    checksum_expected, checksum_actual, checksum_ok
    FROM mc_authority WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3";

#[derive(Debug)]
enum AuthorityTransitionError {
    State { expected: String, found: String },
    Generation { expected: u64, found: u64 },
    CoordinatorToken,
    CoordinatorLeaseExpired,
    UnresolvedPendingReferences { count: i64 },
}

impl std::fmt::Display for AuthorityTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State { expected, found } => {
                write!(f, "expected state {expected}, found {found}")
            }
            Self::Generation { expected, found } => {
                write!(f, "expected generation {expected}, found {found}")
            }
            Self::CoordinatorToken => {
                write!(f, "drain coordinator token mismatch or missing")
            }
            Self::CoordinatorLeaseExpired => {
                write!(f, "drain coordinator lease expired")
            }
            Self::UnresolvedPendingReferences { count } => {
                write!(
                    f,
                    "authority prepare complete rejected: {count} unresolved pending memory references"
                )
            }
        }
    }
}

fn mint_coordinator_token(lease: &str, lease_expires_at: i64, generation: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}",
        Sha256::digest(
            format!("drain-token:{lease}:{lease_expires_at}:{generation}:{nanos}").as_bytes()
        )
    )
}

impl std::error::Error for AuthorityTransitionError {}

fn map_authority_sql_error(error: StoreError) -> McStoreError {
    // cortexkit-store intentionally erases driver-specific errors at its connection
    // boundary. Keep the durable operation error rather than pretending it was a
    // successful transition when a generation or state check failed.
    McStoreError::Store(error)
}

fn validate_authority_domain(domain: &str) -> Result<(), McStoreError> {
    if matches!(domain, "memories" | "notes") {
        Ok(())
    } else {
        Err(McStoreError::Serde(format!(
            "unknown authority domain {domain}"
        )))
    }
}

fn authority_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthorityRow> {
    Ok(AuthorityRow {
        context_store_uuid: row.get(0)?,
        project: row.get(1)?,
        domain: row.get(2)?,
        state: row.get(3)?,
        generation: row.get::<_, i64>(4)? as u64,
        captured_upper_bound: row.get(5)?,
        drain_generation: row.get::<_, Option<i64>>(6)?.map(|value| value as u64),
        drain_cursor: row.get(7)?,
        step_seed: row.get::<_, i64>(8)? != 0,
        step_memories: row.get::<_, i64>(9)? != 0,
        step_notes: row.get::<_, i64>(10)? != 0,
        step_compartments: row.get::<_, i64>(11)? != 0,
        step_reconcile: row.get::<_, i64>(12)? != 0,
        step_verify: row.get::<_, i64>(13)? != 0,
        step_flip: row.get::<_, i64>(14)? != 0,
        coordinator_lease: row.get(15)?,
        lease_expires_at: row.get(16)?,
        coordinator_token: row.get(17)?,
        checksum_expected: row.get(18)?,
        checksum_actual: row.get(19)?,
        checksum_ok: row.get::<_, Option<i64>>(20)?.map(|value| value != 0),
    })
}

fn authority_require_live_coordinator(
    current: &AuthorityRow,
    expected_token: &str,
    now_ms: i64,
) -> Result<(), rusqlite::Error> {
    if current.coordinator_token.as_deref() != Some(expected_token) || expected_token.is_empty() {
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            AuthorityTransitionError::CoordinatorToken,
        )));
    }
    if current.lease_expires_at.unwrap_or(0) <= now_ms {
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            AuthorityTransitionError::CoordinatorLeaseExpired,
        )));
    }
    Ok(())
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
             UNION ALL SELECT 1 FROM mc_temporal_marks WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_overlay_frontiers WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_wrapup_commands WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_recomp_commands WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_pass_trace WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_chunk_transcripts WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_compartment_events WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_primer_candidates WHERE session_id = ?1
             UNION ALL SELECT 1 FROM mc_user_memory_candidates WHERE session_id = ?1
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
    AuthoritySeqMismatch { found: u64 },
    InvalidSeedBoundary { declared: String, detail: String },
    Serde(String),
}

enum ShadowDivergenceTxnOutcome {
    Committed(ShadowDivergenceWriteResult),
    GenerationMismatch { found: u64 },
    Serde(String),
}

#[cfg(any(test, feature = "test-support"))]
type AbandonHistorianHook = std::sync::Arc<std::sync::Mutex<Option<Box<dyn FnMut() + Send>>>>;

/// The Magic Context cache-state store: one single-writer SQLite handle for the
/// module's lifetime.
struct FacadeAuthorityScope {
    owner: std::thread::ThreadId,
    route_project_root: String,
    domain: String,
}

struct FacadeMutationScopeGuard<'a> {
    scope: &'a Mutex<Option<FacadeAuthorityScope>>,
}

impl Drop for FacadeMutationScopeGuard<'_> {
    fn drop(&mut self) {
        *self
            .scope
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

pub struct McStore {
    inner: SqliteStore,
    /// The connection-local caller identity used by note ownership triggers. It is
    /// installed only while a fenced note mutation is executing, so an unwrapped SQL
    /// writer fails closed instead of inheriting a previous operation's project.
    note_caller_project: Arc<Mutex<Option<String>>>,
    /// Facade scope is visible to SQLite triggers for the duration of a mutation. A separate
    /// lock serializes scopes so one request cannot lend its authority identity to another.
    facade_authority_scope: Arc<Mutex<Option<FacadeAuthorityScope>>>,
    facade_mutation_lock: Mutex<()>,
    #[cfg(any(test, feature = "test-support"))]
    abandon_historian_hook: AbandonHistorianHook,
    #[cfg(any(test, feature = "test-support"))]
    authority_project_resolution_fail_once: std::sync::atomic::AtomicBool,
}

fn valid_drop_seed_block_id(block_id: &str) -> bool {
    let Some((mid, index)) = block_id.rsplit_once('#') else {
        return false;
    };
    !mid.is_empty() && !mid.contains('#') && index.parse::<usize>().is_ok()
}

fn seeded_drop_unit(
    block_id: &str,
    drop_mode: &str,
    payload: Option<&str>,
    related: bool,
) -> Option<FrozenUnit> {
    if !valid_drop_seed_block_id(block_id) {
        return None;
    }
    let (kind, frozen_payload) = if related || drop_mode == "full" {
        ("drop", "[dropped]".to_string())
    } else if drop_mode == "truncated" || drop_mode == "skeleton" {
        ("skeleton", "[dropped]".to_string())
    } else if drop_mode == "edit_marker" {
        ("edit_marker", payload.unwrap_or("[dropped]").to_string())
    } else {
        return None;
    };
    Some(FrozenUnit {
        key: format!("red:{block_id}"),
        kind: kind.to_string(),
        frozen_payload,
        durability_class: DurabilityClass::Lineage,
        reset_rule: String::new(),
    })
}

/// Materialize the TypeScript drop snapshot before the first transform. A tag
/// may name a tool arc, so its paired result receives the module's normal drop
/// unit while the call block keeps the requested skeleton or edit marker kind.
fn materialize_drop_seed_units(
    core: &mut CoreState,
    session_id: &str,
    seeds: &[ShadowDropSeedRow],
    initial_skipped: usize,
) -> usize {
    let mut skipped = initial_skipped;
    let mut candidates = BTreeMap::<String, FrozenUnit>::new();
    for seed in seeds {
        let Some(primary) = seeded_drop_unit(
            &seed.block_id,
            &seed.drop_mode,
            seed.payload.as_deref(),
            false,
        ) else {
            skipped = skipped.saturating_add(1);
            eprintln!(
                "mc-store: skipped invalid drop seed for session {session_id}: {}",
                seed.block_id
            );
            continue;
        };
        let primary_key = primary.key.clone();
        if let Some(existing) = candidates.get(&primary_key) {
            if existing != &primary {
                let existing_order = (&existing.kind, &existing.frozen_payload);
                let primary_order = (&primary.kind, &primary.frozen_payload);
                if primary_order < existing_order {
                    candidates.insert(primary_key.clone(), primary);
                }
                eprintln!(
                    "mc-store: resolved conflicting drop seed deterministically for session {session_id}: {}",
                    primary_key
                );
            }
        } else {
            candidates.insert(primary_key, primary);
        }
        let mut related = seed.related_block_ids.clone();
        related.sort();
        related.dedup();
        for block_id in related {
            let Some(unit) = seeded_drop_unit(&block_id, "full", None, true) else {
                skipped = skipped.saturating_add(1);
                eprintln!(
                    "mc-store: skipped invalid related drop seed for session {session_id}: {block_id}"
                );
                continue;
            };
            if let Some(existing) = candidates.get(&unit.key) {
                if existing != &unit {
                    eprintln!(
                        "mc-store: ignored conflicting related drop seed for session {session_id}: {}",
                        unit.key
                    );
                }
            } else {
                candidates.insert(unit.key.clone(), unit);
            }
        }
    }

    for (key, unit) in candidates {
        if let Some(existing) = core
            .frozen_units
            .iter()
            .find(|existing| existing.key == key)
        {
            if existing != &unit {
                eprintln!(
                    "mc-store: retained existing frozen drop unit for session {session_id}: {key}"
                );
            }
            continue;
        }
        core.frozen_units.push(unit);
    }
    skipped
}

impl McStore {
    pub fn open(descriptor: &StorageDescriptor) -> Result<Self, McStoreError> {
        let inner = open_sqlite(descriptor)?;
        let note_caller_project = Arc::new(Mutex::new(None::<String>));
        let facade_authority_scope = Arc::new(Mutex::new(None::<FacadeAuthorityScope>));
        let note_udf_scope = Arc::clone(&note_caller_project);
        let facade_domain_scope = Arc::clone(&facade_authority_scope);
        let facade_route_scope = Arc::clone(&facade_authority_scope);
        // Register before migrations: migrations create triggers that call these functions,
        // and the same connection must expose them before the first guarded write is possible.
        inner.with_conn(move |conn| {
            conn.create_scalar_function(
                "mc_note_caller_project",
                0,
                FunctionFlags::SQLITE_UTF8,
                move |_context| {
                    Ok(note_udf_scope
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                        .unwrap_or_default())
                },
            )?;
            conn.create_scalar_function(
                "mc_facade_authority_domain",
                0,
                FunctionFlags::SQLITE_UTF8,
                move |_context| {
                    Ok(facade_domain_scope
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .filter(|scope| scope.owner == std::thread::current().id())
                        .map(|scope| scope.domain.clone())
                        .unwrap_or_default())
                },
            )?;
            conn.create_scalar_function(
                "mc_facade_authority_route",
                0,
                FunctionFlags::SQLITE_UTF8,
                move |_context| {
                    Ok(facade_route_scope
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .filter(|scope| scope.owner == std::thread::current().id())
                        .map(|scope| scope.route_project_root.clone())
                        .unwrap_or_default())
                },
            )
        })?;
        inner.migrate(NS, MIGRATIONS)?;
        let store = McStore {
            inner,
            note_caller_project,
            facade_authority_scope,
            facade_mutation_lock: Mutex::new(()),
            #[cfg(any(test, feature = "test-support"))]
            abandon_historian_hook: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(any(test, feature = "test-support"))]
            authority_project_resolution_fail_once: std::sync::atomic::AtomicBool::new(false),
        };
        store.repair_migration_30_authority_routes()?;
        store.prune_transform_session_roots()?;
        Ok(store)
    }

    fn prune_transform_session_roots(&self) -> Result<(), McStoreError> {
        const THIRTY_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1000;
        let now_ms = current_time_ms();
        self.inner
            .with_conn(|conn| {
                // Cache rows are retained across session teardown, so row existence is not
                // activity. Prune lineage only when both the root observation and its cache
                // activity watermark are older than the inactivity window. This keeps active
                // sessions alive without depending on a deletion path that does not exist.
                conn.execute(
                    "DELETE FROM mc_transform_session_roots AS roots
                      WHERE roots.observed_at < ?1
                        AND NOT EXISTS (
                            SELECT 1 FROM mc_cache_state AS cache
                             WHERE cache.session_id = roots.session_id
                               AND cache.last_activity_at >= ?1
                        )",
                    params![now_ms.saturating_sub(THIRTY_DAYS_MS)],
                )?;
                Ok(())
            })
            .map_err(Into::into)
    }

    /// Recover the authenticated transform lineage after a module process restart. Cache state
    /// alone is insufficient: the exact project root must have been committed with that session.
    pub fn knows_transform_session_root(
        &self,
        session_id: &str,
        project_root: &str,
    ) -> Result<bool, McStoreError> {
        let candidate = canonical_root(project_root);
        self.inner
            .with_conn(|conn| {
                // Older stores may contain symlink spellings. Load the tiny per-session set and
                // canonicalize both sides in Rust rather than relying on SQL string equality;
                // this keeps migration compatibility without changing the durable schema.
                let mut statement = conn.prepare(
                    "SELECT project_root
                       FROM mc_transform_session_roots
                      WHERE session_id = ?1",
                )?;
                let rows =
                    statement.query_map(params![session_id], |row| row.get::<_, String>(0))?;
                for row in rows {
                    if canonical_root(row?) == candidate {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .map_err(Into::into)
    }

    /// Run one facade mutation while SQLite triggers can verify the route's authority state in
    /// the mutation transaction. The scope is cleared on every normal return before another
    /// facade mutation can enter.
    pub fn with_facade_mutation<T, E>(
        &self,
        route_project_root: &str,
        domain: &str,
        mutation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let _mutation_guard = self
            .facade_mutation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut scope = self
                .facade_authority_scope
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *scope = Some(FacadeAuthorityScope {
                owner: std::thread::current().id(),
                route_project_root: route_project_root.to_string(),
                domain: domain.to_string(),
            });
        }
        let _scope_guard = FacadeMutationScopeGuard {
            scope: &self.facade_authority_scope,
        };
        mutation()
    }

    /// Complete route normalization after schema upgrades using the same caller-identity
    /// check as runtime note writes. Because the SQL migration cannot safely rekey several
    /// note owners under one caller identity, replay this idempotent repair on every store
    /// open, including stores that already recorded the upgraded schema version.
    fn repair_migration_30_authority_routes(&self) -> Result<(), McStoreError> {
        let bindings = self.inner.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT binding.context_store_uuid, binding.project, binding.route_project_root
                   FROM mc_authority_route_bindings binding
                  WHERE EXISTS (
                        SELECT 1 FROM mc_authority authority
                         WHERE authority.context_store_uuid = binding.context_store_uuid
                           AND authority.project = binding.project
                           AND authority.state = 'MODULE'
                           AND authority.domain IN ('memories', 'notes')
                  )
                  ORDER BY binding.route_project_root",
            )?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<(String, String, String)>, _>>()?;
            Ok(rows)
        })?;
        for (context_store_uuid, project, route_project_root) in bindings {
            self.with_note_conn_fenced(&route_project_root, |tx| {
                normalize_authority_route_tx(tx, &context_store_uuid, &project, &route_project_root)
            })?;
        }
        Ok(())
    }

    /// Associate a daemon-bound route root with an authority identity. The route path
    /// is transport vocabulary; the identity remains the key used by domain rows.
    pub fn bind_authority_route(
        &self,
        context_store_uuid: &str,
        project: &str,
        route_project_root: &str,
    ) -> Result<(), McStoreError> {
        self.with_note_conn_fenced(route_project_root, |tx| {
            tx.execute(
                "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(route_project_root) DO UPDATE SET
                    context_store_uuid = excluded.context_store_uuid,
                    project = excluded.project",
                params![route_project_root, context_store_uuid, project],
            )?;
            // The trigger remains the direct-SQL safety net, but cleanup also belongs to
            // this operation: a bind can happen before twins exist and never be retried
            // by the cached authority-status path. Running it after every upsert makes
            // the vocabulary law independent of write ordering.
            normalize_authority_route_tx(tx, context_store_uuid, project, route_project_root)?;
            Ok(())
        })
    }

    /// Resolve a requested facade identity to the MODULE authority that owns it. The
    /// context UUID is returned with the identity so a write can establish the route
    /// binding even when the transform's authority-status result was cached.
    pub fn module_authority_for_project(
        &self,
        project: &str,
        domain: &str,
    ) -> Result<Option<(String, String)>, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT context_store_uuid, project
                       FROM mc_authority
                      WHERE project = ?1 AND domain = ?2 AND state = 'MODULE'
                      ORDER BY context_store_uuid
                      LIMIT 1",
                    params![project, domain],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .map_err(Into::into)
    }

    /// Resolve a facade-supplied identity while it is module-owned or draining. DRAINING remains
    /// visible so callers can return a retryable transition error instead of falling back to a
    /// filesystem route.
    pub fn facade_authority_for_project(
        &self,
        project: &str,
        domain: &str,
    ) -> Result<Option<(String, String, String)>, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT context_store_uuid, project, state
                       FROM mc_authority
                      WHERE project = ?1 AND domain = ?2
                        AND state IN ('MODULE', 'DRAINING')
                      ORDER BY context_store_uuid
                      LIMIT 1",
                    params![project, domain],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
            })
            .map_err(Into::into)
    }

    /// Return the authority identity and state bound to this daemon route while ownership is
    /// active or draining. Mutation callers use the state; transforms and reads keep continuity.
    pub fn authority_project_state_for_route(
        &self,
        route_project_root: &str,
        domain: &str,
    ) -> Result<Option<(String, String)>, McStoreError> {
        validate_authority_domain(domain)?;
        #[cfg(any(test, feature = "test-support"))]
        if self
            .authority_project_resolution_fail_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(McStoreError::Serde(
                "injected authority project resolution failure".to_string(),
            ));
        }
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT authority.project, authority.state
                       FROM mc_authority_route_bindings binding
                       JOIN mc_authority authority
                         ON authority.context_store_uuid = binding.context_store_uuid
                        AND authority.project = binding.project
                      WHERE binding.route_project_root = ?1
                        AND authority.domain = ?2
                        AND authority.state IN ('MODULE', 'DRAINING')",
                    params![route_project_root, domain],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .map_err(Into::into)
    }

    /// Return the authority identity bound to this daemon route only while ownership is active
    /// or draining. PREPARING remains route-keyed so transforms cannot observe a partial seed;
    /// the verified MODULE acknowledgement publishes the identity-key flip.
    pub fn authority_project_for_route(
        &self,
        route_project_root: &str,
        domain: &str,
    ) -> Result<Option<String>, McStoreError> {
        validate_authority_domain(domain)?;
        #[cfg(any(test, feature = "test-support"))]
        if self
            .authority_project_resolution_fail_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(McStoreError::Serde(
                "injected authority project resolution failure".to_string(),
            ));
        }
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT authority.project
                       FROM mc_authority_route_bindings binding
                       JOIN mc_authority authority
                         ON authority.context_store_uuid = binding.context_store_uuid
                        AND authority.project = binding.project
                      WHERE binding.route_project_root = ?1
                        AND authority.domain = ?2
                        AND authority.state IN ('MODULE', 'DRAINING')",
                    params![route_project_root, domain],
                    |row| row.get(0),
                )
                .optional()
            })
            .map_err(Into::into)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_authority_project_resolution_for_test(&self) {
        self.authority_project_resolution_fail_once
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Reject a facade write that crosses the route's active authority identity.
    pub fn enforce_facade_project_vocabulary(
        &self,
        route_project_root: &str,
        write_project: &str,
        domain: &str,
    ) -> Result<(), McStoreError> {
        let authority_project = self.authority_project_for_route(route_project_root, domain)?;
        if let Some(authority_project) = authority_project.filter(|value| value != write_project) {
            return Err(McStoreError::FacadeProjectVocabularyMismatch {
                route_project_root: route_project_root.to_string(),
                authority_project,
                write_project: write_project.to_string(),
                domain: domain.to_string(),
            });
        }
        Ok(())
    }

    /// Install a test callback while cleanup of a matching pending historian run holds
    /// SQLite's writer lock. The callback checks that a competing write cannot slip
    /// between reading the match and storing the idle state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_abandon_historian_hook(&self, hook: Box<dyn FnMut() + Send>) {
        *self
            .abandon_historian_hook
            .lock()
            .expect("abandon historian hook mutex") = Some(hook);
    }

    fn with_note_conn_fenced<T>(
        &self,
        caller_project: &str,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, McStoreError> {
        let caller_project = caller_project.to_string();
        let caller_scope = Arc::clone(&self.note_caller_project);
        self.inner
            .with_conn_fenced(|tx| {
                let previous = caller_scope
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .replace(caller_project);
                let result = operation(tx);
                *caller_scope
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous;
                result
            })
            .map_err(Into::into)
    }

    /// Whether a session has committed module cache state. Facade identity shortcuts use
    /// this as a provenance check; a client-supplied harness label cannot create this row.
    pub fn has_cache_state(&self, session_id: &str) -> Result<bool, McStoreError> {
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM mc_cache_state WHERE session_id = ?1)",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map(|exists| exists != 0)
            .map_err(Into::into)
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

    /// Load cache state and all render overlays from one SQLite read transaction.
    /// No-write passes use this snapshot as their read linearization point.
    pub fn load_transform_snapshot(
        &self,
        session_id: &str,
    ) -> Result<TransformSnapshot, McStoreError> {
        self.load_transform_snapshot_with_hook(session_id, || {})
    }

    fn load_transform_snapshot_with_hook(
        &self,
        session_id: &str,
        after_state_read: impl FnOnce(),
    ) -> Result<TransformSnapshot, McStoreError> {
        let snapshot = self.inner.with_conn(|conn| {
            let transaction = conn.unchecked_transaction()?;
            let state = transaction
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)? as u64,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let loaded = match state {
                Some((row_version, core_json, meta_json)) => LoadedState {
                    core: serde_json::from_str(&core_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    meta: serde_json::from_str(&meta_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    row_version: Some(row_version),
                },
                None => LoadedState {
                    core: CoreState::default(),
                    meta: ModuleMeta::default(),
                    row_version: None,
                },
            };
            after_state_read();

            let tags = {
                let mut statement = transaction.prepare(
                    "SELECT tag_number, block_id, kind, token_count, created_at_ms, source_bytes
                     FROM mc_tags WHERE session_id = ?1 ORDER BY tag_number ASC",
                )?;
                let rows = statement
                    .query_map(params![session_id], tag_row_from_sql)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let temporal_marks = {
                let mut statement = transaction.prepare(
                    "SELECT block_id, marker_text, created_at FROM mc_temporal_marks
                     WHERE session_id = ?1 ORDER BY created_at ASC, block_id ASC",
                )?;
                let rows = statement
                    .query_map(params![session_id], |row| {
                        Ok(TemporalMarkRow {
                            block_id: row.get(0)?,
                            marker_text: row.get(1)?,
                            created_at: row.get(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let user_hints = {
                let mut statement = transaction.prepare(
                    "SELECT block_id, hint_text, created_at FROM mc_user_hints
                     WHERE session_id = ?1 ORDER BY created_at ASC, block_id ASC",
                )?;
                let rows = statement
                    .query_map(params![session_id], |row| {
                        Ok(UserHintRow {
                            block_id: row.get(0)?,
                            hint_text: row.get(1)?,
                            created_at: row.get(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let channel1_appends = {
                let mut statement = transaction.prepare(
                    "SELECT block_id, reminder_text, fired_at_ms FROM mc_channel1_appends
                     WHERE session_id = ?1 ORDER BY fired_at_ms ASC, block_id ASC",
                )?;
                let rows = statement
                    .query_map(params![session_id], |row| {
                        Ok(Channel1AppendRow {
                            block_id: row.get(0)?,
                            reminder_text: row.get(1)?,
                            fired_at_ms: row.get(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let overlay_frontier = transaction
                .query_row(
                    "SELECT max_seen_ordinal FROM mc_overlay_frontiers WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .map(|ordinal| ordinal.max(0) as u64);
            transaction.commit()?;
            Ok(TransformSnapshot {
                loaded,
                tags,
                temporal_marks,
                user_hints,
                channel1_appends,
                overlay_frontier,
            })
        })?;
        Ok(snapshot)
    }

    /// Load all durable fields used by session.status from one SQLite read transaction.
    pub fn load_session_status_snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionStatusSnapshot, McStoreError> {
        let snapshot = self.inner.with_conn(|conn| {
            let transaction = conn.unchecked_transaction()?;
            let state = transaction
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)? as u64,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let loaded = match state {
                Some((row_version, core_json, meta_json)) => LoadedState {
                    core: serde_json::from_str(&core_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    meta: serde_json::from_str(&meta_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    row_version: Some(row_version),
                },
                None => LoadedState {
                    core: CoreState::default(),
                    meta: ModuleMeta::default(),
                    row_version: None,
                },
            };
            let count = |table: &str| -> Result<usize, rusqlite::Error> {
                transaction.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .map(|value| value.max(0) as usize)
            };
            let pass_trace = transaction
                .query_row(
                    "SELECT last_received_at_ms, last_completed_at_ms, last_reject_error,
                            last_reject_at_ms, reject_count, receive_count
                       FROM mc_pass_trace WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        Ok(PassTrace {
                            last_received_at_ms: row.get(0)?,
                            last_completed_at_ms: row.get(1)?,
                            last_reject_error: row.get(2)?,
                            last_reject_at_ms: row.get(3)?,
                            reject_count: row.get::<_, i64>(4)?.max(0) as u64,
                            receive_count: row.get::<_, i64>(5)?.max(0) as u64,
                        })
                    },
                )
                .optional()?;
            let snapshot = SessionStatusSnapshot {
                loaded,
                compartment_count: count("mc_compartments")?,
                pending_drop_count: count("pending_agent_drops")?,
                tag_count: count("mc_tags")?,
                pass_trace,
            };
            transaction.commit()?;
            Ok(snapshot)
        })?;
        Ok(snapshot)
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
            false,
        )?;
        Ok(outcome.queued as usize)
    }

    /// Append ctx_reduce drops and, when supplied, durably record the command that requested
    /// them. A repeated command is acknowledged without touching pending queue rows.
    ///
    /// When `zero_targets` is true the ledger row is still recorded (idempotency correctness
    /// requires it — a retry of the same command_id must still dedupe), but the disposition
    /// is set to "no_targets" so the row is not counted as pending.
    pub fn append_pending_agent_drops_with_command(
        &self,
        session_id: &str,
        command_id: Option<&str>,
        target_ids: &[String],
        queued_at_ms: i64,
        zero_targets: bool,
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
                        disposition: None,
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
                    "INSERT OR IGNORE INTO pending_agent_drops
                         (session_id, target_id, queued_at, command_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session_id, target_id, queued_at_ms, command_id],
                )? as u64;
            }

            // Command ids are lineage-durable. Pruning would make an old outcome-unknown
            // retry destructive again, so rows leave only with real lineage teardown.

            // When the caller resolved zero targets, mark the ledger row as terminal so it
            // is not counted among pending commands. The row still exists for idempotency.
            if zero_targets {
                if let Some(command_id) = command_id {
                    tx.execute(
                        "UPDATE mc_reduce_command_ledger
                         SET disposition = 'no_targets'
                         WHERE session_id = ?1
                           AND command_id = ?2
                           AND disposition IS NULL",
                        params![session_id, command_id],
                    )?;
                }
            }

            Ok(AppendOutcome {
                queued,
                duplicate: false,
                disposition: if zero_targets {
                    Some("no_targets".to_string())
                } else {
                    None
                },
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
                "SELECT p.id, p.target_id, p.queued_at, p.command_id,
                        l.first_applied_at_ms
                 FROM pending_agent_drops p
                 LEFT JOIN mc_reduce_command_ledger l
                   ON l.session_id = p.session_id AND l.command_id = p.command_id
                 WHERE p.session_id = ?1
                 ORDER BY p.queued_at ASC, p.id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |r| {
                Ok(PendingAgentDrop {
                    id: r.get(0)?,
                    target_id: r.get(1)?,
                    queued_at_ms: r.get(2)?,
                    command_id: r.get(3)?,
                    command_first_applied_at_ms: r.get(4)?,
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
    // Production callers live module-side behind the test-support seeds; without that
    // feature only in-crate tests reach these write paths, so silence the lint there.
    #[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
    pub(crate) fn mint_or_get_tags(
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
    #[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
    pub(crate) fn append_channel1_nudge(
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

    /// Read the ordinal frontier used to avoid first-applying overlays to closed turns.
    /// A missing row is distinct from ordinal zero, which is a valid first message.
    pub fn overlay_watermark(&self, session_id: &str) -> Result<Option<u64>, McStoreError> {
        let value = self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT max_seen_ordinal FROM mc_overlay_frontiers WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })?;
        Ok(value.map(|ordinal| ordinal.max(0) as u64))
    }

    /// Freeze first-sight overlay decisions and advance the pass watermark atomically.
    /// Every candidate is compared with the watermark from before this transaction. A
    /// racing hint writer returns the already-stored bytes so both callers render the
    /// same canonical decision.
    #[cfg(test)]
    pub(crate) fn apply_active_overlay_decisions(
        &self,
        session_id: &str,
        max_seen_ordinal: u64,
        temporal_marks: &[TemporalMarkInput],
        user_hint: Option<&UserHintDecisionInput>,
        created_at: i64,
    ) -> Result<Option<UserHintRow>, McStoreError> {
        let max_seen_ordinal = i64::try_from(max_seen_ordinal)
            .map_err(|_| McStoreError::Serde("message ordinal exceeds SQLite range".to_string()))?;
        let temporal_marks = temporal_marks
            .iter()
            .map(|mark| {
                i64::try_from(mark.ordinal)
                    .map(|ordinal| (ordinal, mark))
                    .map_err(|_| {
                        McStoreError::Serde("message ordinal exceeds SQLite range".to_string())
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let hint_ordinal = user_hint
            .map(|hint| {
                i64::try_from(hint.ordinal).map_err(|_| {
                    McStoreError::Serde("message ordinal exceeds SQLite range".to_string())
                })
            })
            .transpose()?;

        Ok(self.inner.with_conn_fenced(|tx| {
            let previous = tx
                .query_row(
                    "SELECT max_seen_ordinal
                     FROM mc_overlay_frontiers
                     WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;

            for (ordinal, mark) in &temporal_marks {
                if previous.is_none_or(|frontier| *ordinal > frontier) {
                    tx.execute(
                        "INSERT OR IGNORE INTO mc_temporal_marks
                             (session_id, block_id, marker_text, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![session_id, mark.block_id, mark.marker_text, created_at],
                    )?;
                }
            }

            let canonical_hint = if let Some(hint) = user_hint {
                let existing = tx
                    .query_row(
                        "SELECT block_id, hint_text, created_at
                         FROM mc_user_hints
                         WHERE session_id = ?1 AND block_id = ?2",
                        params![session_id, hint.block_id],
                        |row| {
                            Ok(UserHintRow {
                                block_id: row.get(0)?,
                                hint_text: row.get(1)?,
                                created_at: row.get(2)?,
                            })
                        },
                    )
                    .optional()?;
                if existing.is_some()
                    || hint_ordinal
                        .is_some_and(|ordinal| previous.is_some_and(|frontier| ordinal <= frontier))
                {
                    existing
                } else {
                    tx.execute(
                        "INSERT OR IGNORE INTO mc_user_hints
                             (session_id, block_id, hint_text, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![session_id, hint.block_id, hint.hint_text, created_at],
                    )?;
                    tx.query_row(
                        "SELECT block_id, hint_text, created_at
                         FROM mc_user_hints
                         WHERE session_id = ?1 AND block_id = ?2",
                        params![session_id, hint.block_id],
                        |row| {
                            Ok(UserHintRow {
                                block_id: row.get(0)?,
                                hint_text: row.get(1)?,
                                created_at: row.get(2)?,
                            })
                        },
                    )
                    .optional()?
                }
            } else {
                None
            };

            tx.execute(
                "INSERT INTO mc_overlay_frontiers (session_id, max_seen_ordinal)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                     max_seen_ordinal = MAX(max_seen_ordinal, excluded.max_seen_ordinal)",
                params![session_id, max_seen_ordinal],
            )?;
            Ok(canonical_hint)
        })?)
    }

    /// Persist one auto-search decision, including an empty no-result decision.
    #[cfg(test)]
    pub(crate) fn append_user_hint(
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

    #[cfg(feature = "test-support")]
    pub fn seed_tags_for_test(
        &self,
        session_id: &str,
        inputs: &[TagMintInput],
        created_at_ms: i64,
    ) -> Result<Vec<McTagRow>, McStoreError> {
        self.mint_or_get_tags(session_id, inputs, created_at_ms)
    }

    #[cfg(feature = "test-support")]
    pub fn seed_channel1_append_for_test(
        &self,
        session_id: &str,
        block_id: &str,
        reminder_text: &str,
        fired_at_ms: i64,
    ) -> Result<bool, McStoreError> {
        self.append_channel1_nudge(session_id, block_id, reminder_text, fired_at_ms)
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

    /// Load persisted marker bytes, including empty no-marker decisions.
    pub fn load_temporal_marks(
        &self,
        session_id: &str,
    ) -> Result<Vec<TemporalMarkRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT block_id, marker_text, created_at
                 FROM mc_temporal_marks
                 WHERE session_id = ?1
                 ORDER BY created_at ASC, block_id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(TemporalMarkRow {
                    block_id: row.get(0)?,
                    marker_text: row.get(1)?,
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

    /// Set the normalized todo snapshot with replay-safe owner/hash semantics. A cache row
    /// CAS makes a retry harmless even when a transform commits between the read and update.
    pub fn set_todo_state(
        &self,
        session_id: &str,
        state_json: &str,
        owner_message_id: &str,
        state_hash: &str,
    ) -> Result<TodoStateSetOutcome, McStoreError> {
        let mut last_conflict = None;
        for _ in 0..8 {
            let loaded = self.load(session_id)?;
            if loaded.meta.last_todo_state_owner_message_id.as_deref() == Some(owner_message_id)
                && loaded.meta.last_todo_state_hash.as_deref() == Some(state_hash)
            {
                return Ok(TodoStateSetOutcome::Noop);
            }
            let mut meta = loaded.meta;
            meta.last_todo_state = Some(state_json.to_string());
            meta.last_todo_state_owner_message_id = Some(owner_message_id.to_string());
            meta.last_todo_state_hash = Some(state_hash.to_string());
            match self.commit(session_id, loaded.row_version, &loaded.core, &meta) {
                Ok(row_version) => {
                    return Ok(TodoStateSetOutcome::Updated { row_version });
                }
                Err(error @ McStoreError::CasConflict { .. }) => last_conflict = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_conflict.unwrap_or_else(|| {
            McStoreError::Serde("todo state update exceeded CAS retry limit".to_string())
        }))
    }

    /// Arm the durable one-shot refresh consumed by the next eligible transform pass.
    pub fn arm_soft_refresh(&self, session_id: &str) -> Result<bool, McStoreError> {
        let mut last_conflict = None;
        for _ in 0..8 {
            let loaded = self.load(session_id)?;
            if loaded.meta.soft_refresh_pending {
                return Ok(true);
            }
            let mut meta = loaded.meta;
            meta.soft_refresh_pending = true;
            match self.commit(session_id, loaded.row_version, &loaded.core, &meta) {
                Ok(_) => return Ok(true),
                Err(error @ McStoreError::CasConflict { .. }) => last_conflict = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_conflict.unwrap_or_else(|| {
            McStoreError::Serde("soft refresh arm exceeded CAS retry limit".to_string())
        }))
    }

    pub fn load_recomp_command(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<RecompCommandRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT disposition, created_at FROM mc_recomp_commands
                 WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    Ok(RecompCommandRow {
                        disposition: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                },
            )
            .optional()
        })?)
    }

    pub fn record_recomp_command(
        &self,
        session_id: &str,
        command_id: &str,
        disposition: &str,
        created_at: i64,
    ) -> Result<RecompCommandRow, McStoreError> {
        if !matches!(
            disposition,
            "started" | "already_in_progress" | "nothing_to_do"
        ) {
            return Err(McStoreError::Serde(format!(
                "invalid recomp disposition {disposition:?}"
            )));
        }
        Ok(self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO mc_recomp_commands
                 (session_id, command_id, disposition, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, command_id, disposition, created_at],
            )?;
            tx.query_row(
                "SELECT disposition, created_at FROM mc_recomp_commands
                 WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    Ok(RecompCommandRow {
                        disposition: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                },
            )
        })?)
    }

    pub fn load_wrapup_command(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<WrapupCommandRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT disposition, rounds, summary, created_at
                 FROM mc_wrapup_commands
                 WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    let rounds = row.get::<_, i64>(1)?;
                    Ok(WrapupCommandRow {
                        disposition: row.get(0)?,
                        rounds: rounds.max(0) as usize,
                        summary: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
        })?)
    }

    /// Record a terminal wrapup outcome and return the canonical row on a retry race.
    pub fn record_wrapup_command(
        &self,
        session_id: &str,
        command_id: &str,
        disposition: &str,
        rounds: usize,
        summary: &str,
        created_at: i64,
    ) -> Result<WrapupCommandRow, McStoreError> {
        let valid_disposition = matches!(disposition, "completed" | "nothing_to_compact");
        if !valid_disposition {
            return Err(McStoreError::Serde(format!(
                "nonterminal wrapup disposition {disposition:?} cannot be recorded"
            )));
        }
        debug_assert!(valid_disposition);
        let rounds = i64::try_from(rounds)
            .map_err(|_| McStoreError::Serde("wrapup rounds exceed SQLite range".to_string()))?;
        Ok(self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO mc_wrapup_commands
                     (session_id, command_id, disposition, rounds, summary, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    command_id,
                    disposition,
                    rounds,
                    summary,
                    created_at
                ],
            )?;
            tx.query_row(
                "SELECT disposition, rounds, summary, created_at
                 FROM mc_wrapup_commands
                 WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    let rounds = row.get::<_, i64>(1)?;
                    Ok(WrapupCommandRow {
                        disposition: row.get(0)?,
                        rounds: rounds.max(0) as usize,
                        summary: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
        })?)
    }

    /// Return a recorded dream-task result for command-id retry deduplication.
    pub fn load_dream_task_command(
        &self,
        session_id: &str,
        command_id: &str,
    ) -> Result<Option<DreamTaskCommandRow>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT response_json, created_at
                   FROM mc_dream_task_commands
                  WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    Ok(DreamTaskCommandRow {
                        response_json: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                },
            )
            .optional()
        })?)
    }

    /// Record the first terminal dream-task response. INSERT OR IGNORE makes a response-loss
    /// retry replay the original provider outcome instead of executing a second child session.
    pub fn record_dream_task_command(
        &self,
        session_id: &str,
        command_id: &str,
        response_json: &str,
        created_at: i64,
    ) -> Result<DreamTaskCommandRow, McStoreError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO mc_dream_task_commands
                     (session_id, command_id, response_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, command_id, response_json, created_at],
            )?;
            tx.query_row(
                "SELECT response_json, created_at
                   FROM mc_dream_task_commands
                  WHERE session_id = ?1 AND command_id = ?2",
                params![session_id, command_id],
                |row| {
                    Ok(DreamTaskCommandRow {
                        response_json: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                },
            )
        })?)
    }

    /// Apply classifier metadata under the memories authority, updating each row only when its
    /// content hash still matches the supplied hash. Classification does not append to the
    /// mutation log because importance, scope, and shareability are cache-neutral metadata;
    /// the feed trigger still mirrors the full updated row for module-owned consumers.
    pub fn set_memory_classification(
        &self,
        context_store_uuid: &str,
        project: &str,
        authority_generation: u64,
        rows: &[ClassificationUpdate],
        now_ms: i64,
    ) -> Result<ClassificationApplyResult, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let authority: Option<(String, u64)> = tx
                .query_row(
                    "SELECT state, generation FROM mc_authority
                       WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'",
                    params![context_store_uuid, project],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((state, generation)) = authority else {
                return Ok(ClassificationTxnOutcome::AuthorityStateMismatch(
                    "missing memories authority".to_string(),
                ));
            };
            if state != "MODULE" {
                return Ok(ClassificationTxnOutcome::AuthorityStateMismatch(state));
            }
            if generation != authority_generation {
                return Ok(ClassificationTxnOutcome::AuthorityGenerationMismatch(
                    generation,
                ));
            }

            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            for update in rows {
                let Some(memory) = load_memory_full_tx(tx, update.memory_id)? else {
                    rejected.push(ClassificationRejected {
                        memory_id: update.memory_id,
                        reason: "not_found".to_string(),
                    });
                    continue;
                };
                if memory.project_path != project {
                    rejected.push(ClassificationRejected {
                        memory_id: update.memory_id,
                        reason: "not_owned".to_string(),
                    });
                    continue;
                }
                if memory.normalized_hash != update.content_hash_at_prompt {
                    rejected.push(ClassificationRejected {
                        memory_id: update.memory_id,
                        reason: "stale".to_string(),
                    });
                    continue;
                }
                if let Some(scope) = update.scope.as_deref() {
                    if !matches!(scope, "project" | "ecosystem" | "universe") {
                        rejected.push(ClassificationRejected {
                            memory_id: update.memory_id,
                            reason: "invalid_scope".to_string(),
                        });
                        continue;
                    }
                }
                let mut assignments = Vec::new();
                let mut values: Vec<rusqlite::types::Value> = Vec::new();
                if let Some(importance) = update.importance {
                    assignments.push("importance = ?".to_string());
                    values.push(rusqlite::types::Value::Integer(i64::from(
                        importance.clamp(1, 100),
                    )));
                }
                if let Some(scope) = update.scope.as_deref() {
                    assignments.push("scope = ?".to_string());
                    values.push(rusqlite::types::Value::Text(scope.to_string()));
                }
                if let Some(shareable) = update.shareable {
                    assignments.push("shareable = ?".to_string());
                    values.push(rusqlite::types::Value::Integer(if shareable {
                        1
                    } else {
                        0
                    }));
                }
                assignments.push("classified_at = ?".to_string());
                values.push(rusqlite::types::Value::Integer(now_ms));
                values.push(rusqlite::types::Value::Integer(update.memory_id));
                let sql = format!(
                    "UPDATE mc_memories SET {} WHERE id = ?",
                    assignments.join(", ")
                );
                tx.execute(&sql, rusqlite::params_from_iter(values.iter()))?;
                accepted.push(update.memory_id);
            }
            Ok(ClassificationTxnOutcome::Applied(
                ClassificationApplyResult { accepted, rejected },
            ))
        })?;
        match outcome {
            ClassificationTxnOutcome::Applied(result) => Ok(result),
            ClassificationTxnOutcome::AuthorityStateMismatch(found) => {
                Err(McStoreError::AuthorityStateMismatch {
                    expected: "MODULE".to_string(),
                    found,
                })
            }
            ClassificationTxnOutcome::AuthorityGenerationMismatch(found) => {
                Err(McStoreError::AuthorityGenerationMismatch {
                    expected: authority_generation,
                    found,
                })
            }
        }
    }

    /// Apply verification updates only when their authority generation and content hash still match
    /// the values used for classification. Verification-only changes do not invalidate caches, but
    /// content rewrites and archives must be added to the mutation log so consumers see them in the
    /// next incremental update.
    pub fn set_memory_verification(
        &self,
        context_store_uuid: &str,
        project: &str,
        authority_generation: u64,
        rows: &[VerificationUpdate],
        now_ms: i64,
    ) -> Result<VerificationApplyResult, McStoreError> {
        let authority = self.authority_status(context_store_uuid, project, "memories")?;
        let Some(authority) = authority else {
            return Err(McStoreError::AuthorityStateMismatch {
                expected: "MODULE".into(),
                found: "missing".into(),
            });
        };
        if authority.state != "MODULE" {
            return Err(McStoreError::AuthorityStateMismatch {
                expected: "MODULE".into(),
                found: authority.state,
            });
        }
        if authority.generation != authority_generation {
            return Err(McStoreError::AuthorityGenerationMismatch {
                expected: authority_generation,
                found: authority.generation,
            });
        }
        self.inner.with_conn_fenced(|tx| {
            let authority: (String, u64) = tx.query_row(
                "SELECT state, generation FROM mc_authority WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'",
                params![context_store_uuid, project], |row| Ok((row.get(0)?, row.get(1)?)))?;
            if authority.0 != "MODULE" {
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    AuthorityTransitionError::State { expected: "MODULE".into(), found: authority.0 },
                )));
            }
            if authority.1 != authority_generation {
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    AuthorityTransitionError::Generation { expected: authority_generation, found: authority.1 },
                )));
            }
            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            for update in rows {
                let Some(memory) = load_memory_full_tx(tx, update.memory_id)? else {
                    rejected.push(VerificationRejected { memory_id: update.memory_id, reason: "not_found".into() });
                    continue;
                };
                if memory.project_path != project { rejected.push(VerificationRejected { memory_id: update.memory_id, reason: "not_owned".into() }); continue; }
                if memory.normalized_hash != update.content_hash_at_prompt { rejected.push(VerificationRejected { memory_id: update.memory_id, reason: "stale".into() }); continue; }
                match update.verification_status.as_str() {
                    "verified" => { tx.execute("UPDATE mc_memories SET verification_status = 'verified', verified_at = ?1 WHERE id = ?2", params![now_ms, update.memory_id])?; }
                    "update" => {
                        let Some(content) = update.updated_content.as_deref().map(str::trim).filter(|v| !v.is_empty()) else { rejected.push(VerificationRejected { memory_id: update.memory_id, reason: "missing_updated_content".into() }); continue; };
                        let hash = compute_normalized_memory_hash(content);
                        tx.execute("UPDATE mc_memories SET content = ?1, normalized_hash = ?2, updated_at = ?3, shareable = 0, classified_at = NULL, verification_status = 'verified', verified_at = ?3 WHERE id = ?4", params![content, hash, now_ms, update.memory_id])?;
                        tx.execute("DELETE FROM mc_memory_mappings WHERE memory_id = ?1", params![update.memory_id])?;
                        append_memory_mutation_tx(tx, MemoryMutationAppend { project_path: project, mutation_type: "update", target_memory_id: update.memory_id, superseded_by_id: None, category: Some(&memory.category), new_content: Some(content), queued_at: now_ms })?;
                    }
                    "archive" => {
                        let metadata = merge_archive_reason(memory.metadata_json.as_deref(), update.archive_reason.as_deref().unwrap_or("verified archive"));
                        tx.execute("UPDATE mc_memories SET status = 'archived', metadata_json = ?1, updated_at = ?2 WHERE id = ?3", params![metadata, now_ms, update.memory_id])?;
                        tx.execute("DELETE FROM mc_memory_mappings WHERE memory_id = ?1", params![update.memory_id])?;
                        append_memory_mutation_tx(tx, MemoryMutationAppend { project_path: project, mutation_type: "archive", target_memory_id: update.memory_id, superseded_by_id: None, category: None, new_content: None, queued_at: now_ms })?;
                    }
                    _ => { rejected.push(VerificationRejected { memory_id: update.memory_id, reason: "invalid_status".into() }); continue; }
                }
                accepted.push(update.memory_id);
            }
            Ok(VerificationApplyResult { accepted, rejected })
        }).map_err(Into::into)
    }

    /// Store the complete file set reported by map-memories. Mapping is cache-neutral, but the
    /// explicit feed row lets the context mirror converge without treating it as a memory rewrite.
    pub fn set_memory_mapping(
        &self,
        context_store_uuid: &str,
        project: &str,
        authority_generation: u64,
        rows: &[MappingUpdate],
        now_ms: i64,
    ) -> Result<MappingApplyResult, McStoreError> {
        let authority = self.authority_status(context_store_uuid, project, "memories")?;
        let Some(authority) = authority else {
            return Err(McStoreError::AuthorityStateMismatch {
                expected: "MODULE".into(),
                found: "missing".into(),
            });
        };
        if authority.state != "MODULE" {
            return Err(McStoreError::AuthorityStateMismatch {
                expected: "MODULE".into(),
                found: authority.state,
            });
        }
        if authority.generation != authority_generation {
            return Err(McStoreError::AuthorityGenerationMismatch {
                expected: authority_generation,
                found: authority.generation,
            });
        }
        self.inner.with_conn_fenced(|tx| {
            let authority: (String, u64) = tx.query_row(
                "SELECT state, generation FROM mc_authority WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'",
                params![context_store_uuid, project], |row| Ok((row.get(0)?, row.get(1)?)))?;
            if authority.0 != "MODULE" { return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(AuthorityTransitionError::State { expected: "MODULE".into(), found: authority.0 }))); }
            if authority.1 != authority_generation { return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(AuthorityTransitionError::Generation { expected: authority_generation, found: authority.1 }))); }
            tx.execute_batch("CREATE TABLE IF NOT EXISTS mc_memory_mappings (memory_id INTEGER PRIMARY KEY, project_path TEXT NOT NULL, mapped_files_json TEXT NOT NULL, updated_at INTEGER NOT NULL);")?;
            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            for update in rows {
                let Some(memory) = load_memory_full_tx(tx, update.memory_id)? else { rejected.push(MappingRejected { memory_id: update.memory_id, reason: "not_found".into() }); continue; };
                if memory.project_path != project { rejected.push(MappingRejected { memory_id: update.memory_id, reason: "not_owned".into() }); continue; }
                if memory.normalized_hash != update.content_hash_at_prompt { rejected.push(MappingRejected { memory_id: update.memory_id, reason: "stale".into() }); continue; }
                let files = serde_json::to_string(&update.mapped_files).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.execute("INSERT INTO mc_memory_mappings(memory_id, project_path, mapped_files_json, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(memory_id) DO UPDATE SET project_path=excluded.project_path, mapped_files_json=excluded.mapped_files_json, updated_at=excluded.updated_at", params![update.memory_id, project, files, now_ms])?;
                // Mapping is a feed-visible side effect, so it must carry the same complete
                // row shape as the DML trigger. The mapping key is an overlay, not a replacement.
                let snapshot = memory_feed_snapshot(&memory, update.mapped_files.as_deref());
                tx.execute(
                    "INSERT INTO mc_changefeed(domain, op, module_row_id, full_row_snapshot, content_hash) VALUES ('memories', 'update', ?1, ?2, ?3)",
                    params![
                        update.memory_id,
                        serde_json::to_string(&snapshot).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                        update.content_hash_at_prompt,
                    ],
                )?;
                accepted.push(update.memory_id);
            }
            Ok(MappingApplyResult { accepted, rejected })
        }).map_err(Into::into)
    }

    /// Record a terminal wrapup outcome only while the cache row still matches the
    /// state validated by the handler.
    pub fn record_wrapup_command_if_current(
        &self,
        record: WrapupCommandRecord<'_>,
    ) -> Result<RecordWrapupCommandOutcome, McStoreError> {
        // Failed rows can be terminal command results; the module keeps a marker in
        // their summary so older recoverable failure rows retain their retry behavior.
        let valid_disposition = matches!(
            record.disposition,
            "completed" | "nothing_to_compact" | "failed"
        );
        if !valid_disposition {
            return Err(McStoreError::Serde(format!(
                "nonterminal wrapup disposition {:?} cannot be recorded",
                record.disposition
            )));
        }
        debug_assert!(valid_disposition);
        let rounds = i64::try_from(record.rounds)
            .map_err(|_| McStoreError::Serde("wrapup rounds exceed SQLite range".to_string()))?;
        Ok(self.inner.with_conn_fenced(|transaction| {
            let current = transaction
                .query_row(
                    "SELECT row_version, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![record.session_id],
                    |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let (found_row_version, found_revert_epoch) = match current {
                Some((row_version, meta_json)) => {
                    let meta = serde_json::from_str::<ModuleMeta>(&meta_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    (Some(row_version), meta.revert_epoch)
                }
                None => (None, 0),
            };
            if found_row_version != record.expected_row_version
                || found_revert_epoch != record.expected_revert_epoch
            {
                return Ok(RecordWrapupCommandOutcome::Stale {
                    found_row_version,
                    found_revert_epoch,
                });
            }
            let legacy_failure_created_at = transaction
                .query_row(
                    "SELECT disposition, created_at FROM mc_wrapup_commands
                     WHERE session_id = ?1 AND command_id = ?2",
                    params![record.session_id, record.command_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
                .and_then(|(disposition, created_at)| {
                    (disposition == "failed").then_some(created_at)
                });
            if let Some(failed_created_at) = legacy_failure_created_at {
                // A failed audit row is not replayable. Replace it in this fenced
                // transaction so a lost successful response becomes durable idempotency.
                let summary = wrapup_replaced_failure_summary(record.summary, failed_created_at);
                transaction.execute(
                    "UPDATE mc_wrapup_commands
                     SET disposition = ?3, rounds = ?4, summary = ?5, created_at = ?6
                     WHERE session_id = ?1 AND command_id = ?2 AND disposition = 'failed'",
                    params![
                        record.session_id,
                        record.command_id,
                        record.disposition,
                        rounds,
                        summary,
                        record.created_at
                    ],
                )?;
            }

            transaction.execute(
                "INSERT OR IGNORE INTO mc_wrapup_commands
                     (session_id, command_id, disposition, rounds, summary, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.session_id,
                    record.command_id,
                    record.disposition,
                    rounds,
                    record.summary,
                    record.created_at
                ],
            )?;
            let row = transaction.query_row(
                "SELECT disposition, rounds, summary, created_at
                 FROM mc_wrapup_commands
                 WHERE session_id = ?1 AND command_id = ?2",
                params![record.session_id, record.command_id],
                |row| {
                    let rounds = row.get::<_, i64>(1)?;
                    Ok(WrapupCommandRow {
                        disposition: row.get(0)?,
                        rounds: rounds.max(0) as usize,
                        summary: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )?;
            Ok(RecordWrapupCommandOutcome::Recorded(row))
        })?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_legacy_wrapup_command_for_test(
        &self,
        session_id: &str,
        command_id: &str,
        disposition: &str,
        rounds: usize,
        summary: &str,
        created_at: i64,
    ) -> Result<(), McStoreError> {
        let rounds = i64::try_from(rounds)
            .map_err(|_| McStoreError::Serde("wrapup rounds exceed SQLite range".to_string()))?;
        Ok(self.inner.with_conn_fenced(|transaction| {
            transaction.execute(
                "INSERT INTO mc_wrapup_commands
                     (session_id, command_id, disposition, rounds, summary, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    command_id,
                    disposition,
                    rounds,
                    summary,
                    created_at
                ],
            )?;
            Ok(())
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
        self.commit_transform(
            session_id,
            TransformCommit {
                expected,
                core,
                meta,
                consumed_drop_ids,
                first_applied_command_ids: &[],
                memory_revision,
                compartment_max_seq: None,
                project_root: None,
                overlays: TransformOverlayBatch::default(),
            },
        )
    }

    /// Commit accepted cache state and its speculative overlays in one CAS transaction.
    pub fn commit_transform(
        &self,
        session_id: &str,
        request: TransformCommit<'_>,
    ) -> Result<u64, McStoreError> {
        let TransformCommit {
            expected,
            core,
            meta,
            consumed_drop_ids,
            first_applied_command_ids,
            memory_revision,
            compartment_max_seq,
            project_root,
            overlays,
        } = request;
        let max_seen_ordinal = overlays
            .max_seen_ordinal
            .map(|ordinal| {
                i64::try_from(ordinal).map_err(|_| {
                    McStoreError::Serde("message ordinal exceeds SQLite range".to_string())
                })
            })
            .transpose()?;
        let temporal_marks = overlays
            .temporal_marks
            .iter()
            .map(|mark| {
                i64::try_from(mark.ordinal)
                    .map(|ordinal| (ordinal, mark))
                    .map_err(|_| {
                        McStoreError::Serde("message ordinal exceeds SQLite range".to_string())
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let hint_ordinal = overlays
            .user_hint
            .map(|hint| {
                i64::try_from(hint.ordinal).map_err(|_| {
                    McStoreError::Serde("message ordinal exceeds SQLite range".to_string())
                })
            })
            .transpose()?;
        let core_json =
            serde_json::to_string(core).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let meta_json =
            serde_json::to_string(meta).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let next = expected.unwrap_or(0) + 1;
        let canonical_project_root = project_root
            .filter(|root| !root.is_empty())
            .map(|root| canonical_root(root).to_string_lossy().into_owned());

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
            if let Some(expected_seq) = compartment_max_seq {
                let current_seq: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(sequence), 0) FROM mc_compartments WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )?;
                if current_seq != expected_seq {
                    return Ok(CommitOutcome::CasConflict(current.max(0) as u64));
                }
            }

            // INSERT-or-UPDATE in the same fenced txn (bootstrap has no row to UPDATE).
            tx.execute(
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta, last_activity_at)
                  VALUES (?1, ?2, ?3, ?4, ?5)
                  ON CONFLICT(session_id) DO UPDATE SET
                      row_version = excluded.row_version,
                      core_state  = excluded.core_state,
                      meta        = excluded.meta,
                      last_activity_at = excluded.last_activity_at",
                params![session_id, next as i64, core_json, meta_json, current_time_ms()],
            )?;
            if let Some(project_root) = canonical_project_root.as_deref() {
                // Durable root lineage is committed with the cache CAS, so a restart cannot
                // authenticate a root that never produced the accepted session state.
                tx.execute(
                     "INSERT INTO mc_transform_session_roots(
                         session_id, project_root, observed_at
                     ) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id, project_root) DO UPDATE SET
                         observed_at = excluded.observed_at",
                    params![session_id, project_root, overlays.created_at_ms],
                )?;
            }

            for input in overlays.tag_mints {
                let block_id = input.block_id.trim();
                if block_id.is_empty() {
                    continue;
                }
                let exists = tx
                    .query_row(
                        "SELECT 1 FROM mc_tags WHERE session_id = ?1 AND block_id = ?2",
                        params![session_id, block_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if exists {
                    continue;
                }
                let next_tag = tx.query_row(
                    "SELECT COALESCE(MAX(tag_number), 0) + 1 FROM mc_tags WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )?;
                tx.execute(
                    "INSERT INTO mc_tags
                         (session_id, tag_number, block_id, kind, token_count, created_at_ms, source_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        session_id,
                        next_tag,
                        block_id,
                        input.kind,
                        input.token_count.max(0),
                        overlays.created_at_ms,
                        input.source_bytes,
                    ],
                )?;
            }

            let previous_frontier = tx
                .query_row(
                    "SELECT max_seen_ordinal FROM mc_overlay_frontiers WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            for (ordinal, mark) in &temporal_marks {
                if previous_frontier.is_none_or(|frontier| *ordinal > frontier) {
                    tx.execute(
                        "INSERT OR IGNORE INTO mc_temporal_marks
                             (session_id, block_id, marker_text, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            session_id,
                            mark.block_id,
                            mark.marker_text,
                            overlays.created_at_ms
                        ],
                    )?;
                }
            }
            if let Some(hint) = overlays.user_hint {
                let eligible = hint_ordinal.is_some_and(|ordinal| {
                    previous_frontier.is_none_or(|frontier| ordinal > frontier)
                });
                if eligible {
                    tx.execute(
                        "INSERT OR IGNORE INTO mc_user_hints
                             (session_id, block_id, hint_text, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            session_id,
                            hint.block_id,
                            hint.hint_text,
                            overlays.created_at_ms
                        ],
                    )?;
                }
            }
            if let Some(append) = overlays.channel1_append {
                tx.execute(
                    "INSERT OR IGNORE INTO mc_channel1_appends
                         (session_id, block_id, reminder_text, fired_at_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        session_id,
                        append.block_id,
                        append.reminder_text,
                        append.fired_at_ms
                    ],
                )?;
            }
            if let Some(max_seen_ordinal) = max_seen_ordinal {
                tx.execute(
                    "INSERT INTO mc_overlay_frontiers (session_id, max_seen_ordinal)
                     VALUES (?1, ?2)
                     ON CONFLICT(session_id) DO UPDATE SET
                         max_seen_ordinal = MAX(max_seen_ordinal, excluded.max_seen_ordinal)",
                    params![session_id, max_seen_ordinal],
                )?;
            }

            for command_id in first_applied_command_ids {
                tx.execute(
                    "UPDATE mc_reduce_command_ledger
                     SET first_applied_at_ms = ?3
                     WHERE session_id = ?1
                       AND command_id = ?2
                       AND first_applied_at_ms IS NULL",
                    params![session_id, command_id, overlays.created_at_ms],
                )?;
            }
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
        self.apply_state_sync(request, true)
    }

    /// Apply an authority state update atomically after validating its sequence. Authority
    /// rows use the regular memory and profile tables read by real-session transforms, while
    /// compartment and cache-state tables are shared by both lanes.
    pub fn apply_authority_state_sync(
        &self,
        request: ShadowStateSyncRequest<'_>,
    ) -> Result<ShadowStateSyncResult, ShadowStateSyncError> {
        self.apply_state_sync(request, false)
    }

    fn apply_state_sync(
        &self,
        request: ShadowStateSyncRequest<'_>,
        shadow_lane: bool,
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
                return Ok(if shadow_lane {
                    ShadowSyncTxnOutcome::SeqMismatch {
                        found: meta.shadow_seq,
                    }
                } else {
                    ShadowSyncTxnOutcome::AuthoritySeqMismatch {
                        found: meta.shadow_seq,
                    }
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

            let drop_seeds_skipped = materialize_drop_seed_units(
                &mut core,
                request.session_id,
                request.drop_seeds,
                request.drop_seed_skipped,
            );
            let mut pending_agent_drops_seeded = 0usize;
            let mut pending_agent_drops_skipped = request.pending_agent_drops_skipped;
            for seed in request.pending_agent_drops {
                if !valid_drop_seed_block_id(&seed.block_id) {
                    pending_agent_drops_skipped = pending_agent_drops_skipped.saturating_add(1);
                    eprintln!(
                        "mc-store: skipped invalid pending drop seed for session {}: {}",
                        request.session_id, seed.block_id
                    );
                    continue;
                }
                pending_agent_drops_seeded += tx.execute(
                    "INSERT INTO pending_agent_drops(session_id, target_id, queued_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id, target_id) DO NOTHING",
                    params![request.session_id, seed.block_id, seed.queued_at_ms.max(0)],
                )?;
            }
            let mut user_hint_seeds_seeded = 0usize;
            let mut auto_search_hint_skipped = request.auto_search_hint_skipped;
            for seed in request.user_hint_seeds {
                if !valid_drop_seed_block_id(&seed.block_id) {
                    auto_search_hint_skipped = auto_search_hint_skipped.saturating_add(1);
                    continue;
                }
                user_hint_seeds_seeded += tx.execute(
                    "INSERT INTO mc_user_hints(session_id, block_id, hint_text, created_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(session_id, block_id) DO NOTHING",
                    params![request.session_id, seed.block_id, seed.hint_text, current_time_ms()],
                )?;
            }
            let note_nudge_anchors_seeded = request
                .note_nudge_anchors
                .map(|anchors| {
                    meta.note_nudge_anchors = anchors.to_vec();
                    anchors.len()
                })
                .unwrap_or(0);
            let todo_synthetic_anchor_seeded = if request.todo_synthetic_anchor_present {
                meta.synthetic_todo = request.todo_synthetic_anchor.cloned();
                true
            } else {
                false
            };
            let emergency_latches_seeded = if let Some((sample, has_prior, watermark)) = request.emergency_latches {
                meta.last_emergency_input_sample = sample.max(0.0);
                meta.has_prior_emergency_drop = has_prior;
                meta.last_execute_ordinal = watermark;
                true
            } else {
                false
            };

            for compartment in request.compartments {
                upsert_compartment_tx(tx, request.session_id, compartment)?;
            }
            let mut memories_skipped = false;
            if shadow_lane {
                replace_workspace_tx(tx, request.shadow_project_path, request.workspace)?;
                replace_shadow_memories_tx(tx, request.memories)?;
                replace_shadow_memory_mutations_tx(tx, request.memory_mutations)?;
                replace_shadow_user_profile_tx(tx, request.shadow_project_path, request.user_profile)?;
            } else {
                replace_workspace_tx(tx, request.shadow_project_path, request.workspace)?;
                // Each authority pool has exactly one writer. When the module owns memories, this
                // state-sync lane can only mirror module changes back to TypeScript; applying the
                // TypeScript view here would let a stale sender overwrite module-authored fields.
                let memories_authority_state: Option<String> = tx
                    .query_row(
                        "SELECT authority.state
                           FROM mc_authority_route_bindings binding
                           JOIN mc_authority authority
                             ON authority.context_store_uuid = binding.context_store_uuid
                            AND authority.project = binding.project
                          WHERE binding.route_project_root = ?1
                            AND authority.domain = 'memories'",
                        params![request.shadow_project_path],
                        |row| row.get(0),
                    )
                    .optional()?;
                if matches!(
                    memories_authority_state.as_deref(),
                    Some("PREPARING" | "MODULE" | "DRAINING")
                ) {
                    memories_skipped = true;
                } else {
                    replace_authority_memories_tx(
                        tx,
                        request.shadow_project_path,
                        request.memories,
                    )?;
                    replace_authority_memory_mutations_tx(
                        tx,
                        request.shadow_project_path,
                        request.memory_mutations,
                    )?;
                }
                replace_authority_user_profile_tx(tx, request.user_profile)?;
            }

            let in_session_watermark_changed = meta.shadow_acked_watermarks != request.acked_watermarks;
            meta.last_todo_state = request.last_todo_state.clone();
            if in_session_watermark_changed && meta.m1_pending_since_ms.is_none() {
                meta.m1_pending_since_ms = Some(current_time_ms());
            }
            if let Some(epoch) = request.project_memory_epoch {
                if epoch != meta.project_memory_epoch {
                    meta.project_memory_epoch_pending = true;
                }
                meta.project_memory_epoch = epoch;
            }
            if let Some(version) = request.user_profile_version {
                meta.user_profile_version = version;
            }
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
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta, last_activity_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta,
                     last_activity_at = excluded.last_activity_at",
                params![request.session_id, next as i64, core_json, meta_json, current_time_ms()],
            )?;

            Ok(ShadowSyncTxnOutcome::Committed(ShadowStateSyncResult {
                shadow_generation: meta.shadow_generation,
                shadow_seq: meta.shadow_seq,
                row_version: next,
                memories_skipped,
                drop_seeds_skipped,
                pending_agent_drops_seeded,
                pending_agent_drops_skipped,
                user_hint_seeds_seeded,
                auto_search_hint_skipped,
                note_nudge_anchors_seeded,
                todo_synthetic_anchor_seeded,
                emergency_latches_seeded,
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
            ShadowSyncTxnOutcome::AuthoritySeqMismatch { found } => {
                Err(ShadowStateSyncError::AuthoritySeqMismatch {
                    expected: request.expected_shadow_seq,
                    found,
                })
            }
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
                "DELETE FROM mc_wrapup_commands WHERE session_id = ?1",
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
                "DELETE FROM shadow_user_profile WHERE shadow_project_path = ?1",
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
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta, last_activity_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta,
                     last_activity_at = excluded.last_activity_at",
                params![session_id, next as i64, core_json, meta_json, current_time_ms()],
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

    fn stored_compartment_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCompartment> {
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
                .query_map(params![session_id], Self::stored_compartment_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Read the next chronological compartment page and the highest sequence currently
    /// published for the session. The caller supplies the protocol page cap.
    pub fn load_compartments_after(
        &self,
        session_id: &str,
        after_sequence: i64,
        limit: usize,
    ) -> Result<CompartmentPage, McStoreError> {
        let page = self.inner.with_conn(|conn| {
            let max_sequence = conn
                .query_row(
                    "SELECT MAX(sequence) FROM mc_compartments WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, Option<i64>>(0),
                )?
                .map_or(after_sequence, |max| max.max(after_sequence));
            let mut stmt = conn.prepare(
                "SELECT sequence, start_message, end_message, start_message_id, end_message_id,
                        start_date, end_date, title, content, p1, p2, p3, p4, importance,
                        episode_type, legacy, created_at
                 FROM mc_compartments
                 WHERE session_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC LIMIT ?3",
            )?;
            let compartments = stmt
                .query_map(
                    params![session_id, after_sequence, limit as i64],
                    Self::stored_compartment_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompartmentPage {
                compartments,
                max_sequence,
            })
        })?;
        Ok(page)
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
                .query_map(params![session_id], Self::stored_compartment_from_row)?
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

    /// Return the newest note status version for the project. Note readiness is an
    /// in-session m1 input, so it may defer, but it must not be invisible to the next
    /// genuine rendering opportunity.
    pub fn max_note_status_version(&self, project_path: &str) -> Result<i64, McStoreError> {
        let max = self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(status_version), 0) FROM mc_notes WHERE project_path = ?1",
                params![project_path],
                |row| row.get(0),
            )
        })?;
        Ok(max)
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

    /// Reset the module cache to the never-minted boundary used by native recomp.
    ///
    /// This is the full-session form of the revert re-cut: compartments and their
    /// recoverable transcripts are removed, while the cache row is replaced with a
    /// fresh core/meta pair carrying a bumped revert epoch. The epoch and row-version
    /// update share one fenced transaction, so an in-flight historian cannot publish
    /// against the retired compartment set.
    pub fn reset_session_for_recomp(
        &self,
        session_id: &str,
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
                Some(version) => current == version as i64,
                None => current == NO_ROW,
            };
            if !cas_ok {
                return Ok(TruncateTxnOutcome::CasConflict(current.max(0) as u64));
            }
            let prior_meta: ModuleMeta = match serde_json::from_str(&meta_json) {
                Ok(meta) => meta,
                Err(error) => return Ok(TruncateTxnOutcome::Serde(error.to_string())),
            };
            let next_epoch = prior_meta.revert_epoch.saturating_add(1);
            let reset_meta = ModuleMeta {
                revert_epoch: next_epoch,
                last_recut: Some(format!(
                    "native recomp reset all compartments; epoch {next_epoch}"
                )),
                ..ModuleMeta::default()
            };
            let core_json = match serde_json::to_string(&CoreState::default()) {
                Ok(json) => json,
                Err(error) => return Ok(TruncateTxnOutcome::Serde(error.to_string())),
            };
            let reset_meta_json = match serde_json::to_string(&reset_meta) {
                Ok(json) => json,
                Err(error) => return Ok(TruncateTxnOutcome::Serde(error.to_string())),
            };
            tx.execute(
                "DELETE FROM mc_chunk_transcripts WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_compartments WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_compartment_events WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_primer_candidates WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_user_memory_candidates WHERE session_id = ?1",
                params![session_id],
            )?;
            let next_version = current as u64 + 1;
            tx.execute(
                "UPDATE mc_cache_state
                    SET row_version = ?2, core_state = ?3, meta = ?4
                  WHERE session_id = ?1 AND row_version = ?5",
                params![
                    session_id,
                    next_version as i64,
                    core_json,
                    reset_meta_json,
                    current
                ],
            )?;
            Ok(TruncateTxnOutcome::Committed(TruncateOutcome {
                revert_epoch: next_epoch,
                last_recut: reset_meta.last_recut,
                row_version: next_version,
            }))
        })?;
        match outcome {
            TruncateTxnOutcome::Committed(outcome) => Ok(outcome),
            TruncateTxnOutcome::CasConflict(found) => Err(McStoreError::CasConflict {
                expected: expected_row_version,
                found,
            }),
            TruncateTxnOutcome::Serde(error) => Err(McStoreError::Serde(error)),
        }
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
            tx.execute(
                "DELETE FROM mc_compartment_events
                  WHERE session_id = ?1 AND compartment_id > ?2",
                params![session_id, keep_through_seq],
            )?;
            tx.execute(
                "DELETE FROM mc_primer_candidates
                  WHERE session_id = ?1
                    AND source_compartment_start > COALESCE(
                        (SELECT MAX(end_message) FROM mc_compartments WHERE session_id = ?1), -1)",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM mc_user_memory_candidates
                  WHERE session_id = ?1
                    AND source_compartment_start > COALESCE(
                        (SELECT MAX(end_message) FROM mc_compartments WHERE session_id = ?1), -1)",
                params![session_id],
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

    /// Load multiple memory rows by id through the same workspace-visibility predicate the
    /// m0 render path uses. Missing ids are silently absent from the result; rows that the
    /// caller's project identity cannot see (own project always visible; foreign member
    /// memory only when its category is in the workspace's share list) are filtered out
    /// here. Returns a hashmap keyed by memory id, which lets the tool layer preserve the
    /// caller's id order and emit a per-id not-visible report without exposing a foreign
    /// existence oracle.
    pub fn get_visible_memories_by_ids(
        &self,
        project_path: &str,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, StoredMemoryFull>, McStoreError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let membership = self.resolve_workspace_membership(project_path)?;
        let (visibility, visibility_binds): (String, Vec<rusqlite::types::Value>) =
            match &membership {
                Some(m) => workspace_union_memory_visibility_filter(m),
                None => (
                    "project_path = ?".to_string(),
                    vec![rusqlite::types::Value::from(project_path.to_string())],
                ),
            };
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let id_binds: Vec<rusqlite::types::Value> = ids
            .iter()
            .map(|id| rusqlite::types::Value::from(*id))
            .collect();
        // The SQL placeholders appear in `id IN (?,…)` first, then the visibility
        // expression, so positional binds follow the same order.
        let mut binds = id_binds;
        binds.extend(visibility_binds);
        let sql = format!(
            "SELECT id, project_path, category, content, normalized_hash, importance, scope,
                    shareable, source_session_id, source_type, seen_count, retrieval_count,
                    first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
                    status, expires_at, verification_status, verified_at, classified_at,
                    superseded_by_memory_id, merged_from, metadata_json,
                    context_store_uuid, context_row_id
               FROM mc_memories
              WHERE id IN ({placeholders})
                AND ({visibility})"
        );
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    let memory = stored_memory_full_from_row(r)?;
                    Ok((memory.id, memory))
                })?
                .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Insert a memory row unless an existing row already matches the project, category,
    /// and normalized content hash. Duplicate hits update only bookkeeping fields such as
    /// `seen_count` and timestamps, and skip the mutation log because the rendered content
    /// did not change.
    pub fn insert_memory(&self, input: InsertMemoryInput<'_>) -> Result<i64, McStoreError> {
        if let Some(route_project_root) = input.route_project_root {
            self.enforce_facade_project_vocabulary(
                route_project_root,
                input.project_path,
                "memories",
            )?;
        }
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

    /// Return a matching in-flight historian run to Idle in one fenced transaction.
    ///
    /// This operation deliberately has no caller-supplied row version. A publication
    /// failure may race with an unrelated cache-state commit, so the predicate and the
    /// replacement state must both use the row observed after this transaction begins.
    pub fn abandon_historian_run_if_matching(
        &self,
        session_id: &str,
        predicate: &HistorianPublishPredicate,
        failure_backoff_at_ms: Option<i64>,
        detail: Option<&str>,
    ) -> Result<Option<u64>, McStoreError> {
        let outcome = self.inner.with_conn_fenced(|tx| {
            let row = tx
                .query_row(
                    "SELECT row_version, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((current, meta_json)) = row else {
                return Ok(AbandonHistorianTxnOutcome::Unchanged);
            };
            let mut meta: ModuleMeta = match serde_json::from_str(&meta_json) {
                Ok(meta) => meta,
                Err(error) => return Ok(AbandonHistorianTxnOutcome::Serde(error.to_string())),
            };
            let historian = &meta.historian;
            let predicate_matches = historian.firing_seq == predicate.firing_seq
                && historian.producer_run_id.as_deref() == Some(predicate.producer_run_id.as_str())
                && historian.chunk_fingerprint == predicate.chunk_fingerprint;
            if !predicate_matches || historian.state == HistorianPhase::Idle {
                return Ok(AbandonHistorianTxnOutcome::Unchanged);
            }

            #[cfg(any(test, feature = "test-support"))]
            if let Some(hook) = self
                .abandon_historian_hook
                .lock()
                .expect("abandon historian hook mutex")
                .as_mut()
            {
                hook();
            }

            let last_failure = detail
                .map(str::to_string)
                .or_else(|| historian.last_failure.clone());
            meta.historian = HistorianDurableState {
                state: HistorianPhase::Idle,
                firing_seq: historian.firing_seq,
                failure_backoff_at_ms,
                last_failure,
                ..HistorianDurableState::default()
            };
            let next = current.max(0) as u64 + 1;
            let meta_json = match serde_json::to_string(&meta) {
                Ok(json) => json,
                Err(error) => return Ok(AbandonHistorianTxnOutcome::Serde(error.to_string())),
            };
            tx.execute(
                "UPDATE mc_cache_state SET row_version = ?2, meta = ?3
                 WHERE session_id = ?1 AND row_version = ?4",
                params![session_id, next as i64, meta_json, current],
            )?;
            Ok(AbandonHistorianTxnOutcome::Committed(next))
        })?;

        match outcome {
            AbandonHistorianTxnOutcome::Unchanged => Ok(None),
            AbandonHistorianTxnOutcome::Committed(row_version) => Ok(Some(row_version)),
            AbandonHistorianTxnOutcome::Serde(error) => Err(McStoreError::Serde(error)),
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
            let promoted_refs = if request.promote_facts {
                promote_facts_tx(tx, request.project_path, request.facts)?
            } else {
                Vec::new()
            };
            // TS writes extracted events as part of the compartment transaction. Keep the
            // insert best-effort so a malformed side-channel row cannot strand the publish.
            let _ = insert_historian_events_tx(tx, session_id, request.events);

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
            PublishTxnOutcome::Committed(result) => {
                // Primers and user observations are post-commit side channels in the TS
                // publisher. Separate transactions preserve that atomicity boundary.
                let _ = self.persist_historian_side_channels(
                    request.primer_candidates,
                    request.user_memory_candidates,
                );
                Ok(result)
            }
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

    /// Persist the TS publisher's post-commit side channels. Each table gets its own
    /// transaction because observations and primers are intentionally not coupled to the
    /// compartment CAS transaction on the reference path.
    fn persist_historian_side_channels(
        &self,
        primers: &[HistorianPrimerCandidate],
        observations: &[HistorianUserMemoryCandidate],
    ) -> Result<(), McStoreError> {
        if !primers.is_empty() {
            self.inner.with_conn_fenced(|tx| {
                for candidate in primers {
                    let question = candidate.question.trim();
                    if question.is_empty() {
                        continue;
                    }
                    let normalized = question
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase();
                    tx.execute(
                        "INSERT INTO mc_primer_candidates
                           (project_path, harness, session_id, question, normalized_question,
                            source_compartment_start, source_compartment_end,
                            source_start_message_id, source_end_message_id,
                            source_message_time, created_at)
                         VALUES (?1, 'module', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                         ON CONFLICT(project_path, harness, session_id,
                                     source_start_message_id, source_end_message_id)
                         DO UPDATE SET question = excluded.question,
                                       normalized_question = excluded.normalized_question,
                                       source_compartment_start = excluded.source_compartment_start,
                                       source_compartment_end = excluded.source_compartment_end,
                                       source_message_time = excluded.source_message_time,
                                       created_at = MIN(mc_primer_candidates.created_at,
                                                        excluded.created_at)",
                        params![
                            candidate.project_path,
                            candidate.session_id,
                            question,
                            normalized,
                            candidate.source_compartment_start.map(|v| v as i64),
                            candidate.source_compartment_end.map(|v| v as i64),
                            candidate.source_start_message_id,
                            candidate.source_end_message_id,
                            candidate.source_message_time,
                            candidate.created_at,
                        ],
                    )?;
                }
                Ok(())
            })?;
        }
        if !observations.is_empty() {
            self.inner.with_conn_fenced(|tx| {
                for candidate in observations {
                    let content = candidate.content.trim();
                    if content.is_empty() {
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO mc_user_memory_candidates
                           (content, session_id, source_compartment_start,
                            source_compartment_end, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            content,
                            candidate.session_id,
                            candidate.source_compartment_start.map(|v| v as i64),
                            candidate.source_compartment_end.map(|v| v as i64),
                            candidate.created_at,
                        ],
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn load_compartment_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<HistorianEventCandidate>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, at_compartment, compartment_id, fields_json, created_at, harness
                   FROM mc_compartment_events
                  WHERE session_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(HistorianEventCandidate {
                    kind: row.get(0)?,
                    at_compartment: row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    compartment_id: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    fields_json: row.get(3)?,
                    created_at: row.get(4)?,
                    harness: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })?)
    }

    pub fn load_primer_candidates(
        &self,
        session_id: &str,
    ) -> Result<Vec<HistorianPrimerCandidate>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT project_path, session_id, question, source_compartment_start,
                        source_compartment_end, source_start_message_id, source_end_message_id,
                        source_message_time, created_at
                   FROM mc_primer_candidates
                  WHERE session_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(HistorianPrimerCandidate {
                    project_path: row.get(0)?,
                    session_id: row.get(1)?,
                    question: row.get(2)?,
                    source_compartment_start: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    source_compartment_end: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    source_start_message_id: row.get(5)?,
                    source_end_message_id: row.get(6)?,
                    source_message_time: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })?)
    }

    pub fn load_user_memory_candidates(
        &self,
        session_id: &str,
    ) -> Result<Vec<HistorianUserMemoryCandidate>, McStoreError> {
        Ok(self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content, session_id, source_compartment_start,
                        source_compartment_end, created_at
                   FROM mc_user_memory_candidates
                  WHERE session_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                Ok(HistorianUserMemoryCandidate {
                    content: row.get(0)?,
                    session_id: row.get(1)?,
                    source_compartment_start: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    source_compartment_end: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    created_at: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })?)
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
    /// a stable "no workspace" marker). The workspace fingerprint must be deterministic; otherwise
    /// a stable workspace appears to
    /// change on every check and repeatedly invalidates the cache. Visibility corrections use the
    /// m1 removed-delta lane, and expiry is frozen at materialization time rather than included in
    /// workspace identity.
    pub fn workspace_fingerprint(
        &self,
        project_path: &str,
        _now_ms: i64,
    ) -> Result<String, McStoreError> {
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
        // Expiry is frozen at the last materialization timestamp and therefore is
        // intentionally absent from workspace identity. Time passing alone must ride
        // the next natural HARD, not evict the provider cache eagerly.
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

    /// Load the bounded visible pool used by the in-memory lexical hint scorer.
    pub fn load_visible_memory_candidates(
        &self,
        project_path: &str,
        limit: usize,
    ) -> Result<Vec<StoredMemorySearchRow>, McStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let membership = self.resolve_workspace_membership(project_path)?;
        let (sharing, binds) = match &membership {
            Some(membership) => workspace_union_memory_visibility_filter(membership),
            None => project_memory_visibility_filter(project_path),
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, project_path, category, content, updated_at
                   FROM mc_memories
                  WHERE ({sharing})
                    AND status IN ('active', 'permanent')
                    AND (expires_at IS NULL OR expires_at > CAST(strftime('%s', 'now') AS INTEGER) * 1000)
                  ORDER BY updated_at DESC, id ASC
                  LIMIT ?"
            );
            let mut statement = conn.prepare(&sql)?;
            let mut all_binds = binds.clone();
            all_binds.push(rusqlite::types::Value::from(limit));
            let rows = statement
                .query_map(rusqlite::params_from_iter(all_binds.iter()), |row| {
                    Ok(StoredMemorySearchRow {
                        id: row.get(0)?,
                        project_path: row.get(1)?,
                        category: row.get(2)?,
                        content: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Load the bounded session compartment pool used by lexical hint scoring.
    pub fn load_compartment_candidates(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredCompartmentSearchRow>, McStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = self.inner.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT sequence, title, content, p1, p2, p3, p4, created_at
                   FROM mc_compartments
                  WHERE session_id = ?1
                  ORDER BY sequence DESC
                  LIMIT ?2",
            )?;
            let rows = statement
                .query_map(params![session_id, limit], |row| {
                    Ok(StoredCompartmentSearchRow {
                        sequence: row.get(0)?,
                        title: row.get(1)?,
                        content: row.get(2)?,
                        p1: row.get(3)?,
                        p2: row.get(4)?,
                        p3: row.get(5)?,
                        p4: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
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

    fn note_by_id(&self, note_id: i64) -> Result<Option<StoredNote>, McStoreError> {
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    &format!("SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes WHERE id = ?1"),
                    params![note_id],
                    stored_note_from_row,
                )
                .optional()
            })
            .map_err(Into::into)
    }

    fn require_note_project(&self, project_path: &str, note_id: i64) -> Result<(), McStoreError> {
        if let Some(note) = self.note_by_id(note_id)? {
            if note.project_path != project_path {
                return Err(McStoreError::NoteOwnershipMismatch {
                    id: note_id,
                    project: project_path.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Load one note through the same project/session visibility fence used by the facade.
    /// Smart notes are project-visible across sessions; session notes require the provenance
    /// session to match. The SQL predicate keeps this lookup independent of page size.
    pub fn get_note_by_id(
        &self,
        project_path: &str,
        session_id: &str,
        note_id: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    &format!(
                        "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
                         WHERE id = ?1 AND project_path = ?2
                           AND (type = 'smart' OR session_id = ?3)"
                    ),
                    params![note_id, project_path, session_id],
                    stored_note_from_row,
                )
                .optional()
            })
            .map_err(Into::into)
    }

    /// Page the notes visible to one session: all project smart notes plus that session's
    /// ordinary notes. Ownership and pagination both remain in SQL.
    pub fn read_visible_notes(
        &self,
        project_path: &str,
        session_id: &str,
        statuses: &[&str],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredNote>, McStoreError> {
        let limit = i64::try_from(limit.clamp(1, 1000))
            .map_err(|_| McStoreError::Serde("note limit exceeds i64".to_string()))?;
        let offset = i64::try_from(offset)
            .map_err(|_| McStoreError::Serde("note offset exceeds i64".to_string()))?;
        let statuses = if statuses.is_empty() {
            vec!["active"]
        } else {
            statuses.to_vec()
        };
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
             WHERE project_path = ? AND status IN ({placeholders})
               AND (type = 'smart' OR session_id = ?)
             ORDER BY updated_at_ms DESC, id DESC LIMIT ? OFFSET ?"
        );
        self.inner
            .with_conn(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut values = Vec::with_capacity(statuses.len() + 3);
                values.push(SqlValue::Text(project_path.to_string()));
                for status in &statuses {
                    values.push(SqlValue::Text((*status).to_string()));
                }
                values.push(SqlValue::Text(session_id.to_string()));
                values.push(SqlValue::Integer(limit));
                values.push(SqlValue::Integer(offset));
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(values), stored_note_from_row)?
                    .collect::<Result<Vec<_>, _>>();
                rows
            })
            .map_err(Into::into)
    }

    pub fn insert_note(&self, input: NoteInput<'_>) -> Result<StoredNote, McStoreError> {
        if let Some(route_project_root) = input.route_project_root {
            self.enforce_facade_project_vocabulary(
                route_project_root,
                input.project_path,
                "notes",
            )?;
        }
        let content = input.content.trim();
        if content.is_empty() {
            return Err(McStoreError::Serde(
                "note content must not be empty".to_string(),
            ));
        }
        // This compatibility entry point keeps legacy callers on the active-status path
        // used by the previous context-note surface; the full project smart-note writer
        // below deliberately uses pending instead while still recording condition text.
        self.with_note_conn_fenced(input.project_path, |tx| {
            tx.execute(
                "INSERT INTO mc_notes
                 (type, project_path, session_id, content, status, surface_condition,
                  anchor_block_id, harness, created_at_ms, updated_at_ms)
                 VALUES ('session', ?1, ?2, ?3, 'active', ?4, ?5, 'module', ?6, ?6)",
                params![
                    input.project_path,
                    input.session_id,
                    content,
                    input.surface_condition,
                    input.anchor_block_id,
                    input.now_ms,
                ],
            )?;
            load_note_tx(tx, tx.last_insert_rowid())
        })
    }

    pub fn insert_project_note(
        &self,
        input: NoteWriteInput<'_>,
    ) -> Result<StoredNote, McStoreError> {
        if let Some(route_project_root) = input.route_project_root {
            self.enforce_facade_project_vocabulary(
                route_project_root,
                input.project_path,
                "notes",
            )?;
        }
        let content = input.content.trim();
        if content.is_empty() {
            return Err(McStoreError::Serde(
                "note content must not be empty".to_string(),
            ));
        }
        let status = if input
            .surface_condition
            .is_some_and(|condition| !condition.trim().is_empty())
        {
            "pending"
        } else {
            "active"
        };
        self.with_note_conn_fenced(input.project_path, |tx| {
            tx.execute(
                "INSERT INTO mc_notes
                 (type, project_path, session_id, content, status, surface_condition,
                  anchor_block_id, anchor_ordinal, harness, created_at_ms, updated_at_ms)
                 VALUES ('smart', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 'module', ?8, ?8)",
                params![
                    input.project_path,
                    input.session_id,
                    content,
                    status,
                    input
                        .surface_condition
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                    input.anchor_block_id,
                    input.anchor_ordinal,
                    input.now_ms,
                ],
            )?;
            load_note_tx(tx, tx.last_insert_rowid())
        })
    }

    pub fn read_notes(
        &self,
        project_path: &str,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredNote>, McStoreError> {
        self.read_project_notes(project_path, Some(session_id), &["active"], limit, offset)
    }

    /// Read project-owned notes without using session_id as the ownership key. Session id
    /// remains an optional provenance filter for the legacy session-note view only.
    pub fn read_project_notes(
        &self,
        project_path: &str,
        session_id: Option<&str>,
        statuses: &[&str],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredNote>, McStoreError> {
        let limit = i64::try_from(limit.clamp(1, 1000))
            .map_err(|_| McStoreError::Serde("note limit exceeds i64".to_string()))?;
        let offset = i64::try_from(offset)
            .map_err(|_| McStoreError::Serde("note offset exceeds i64".to_string()))?;
        let statuses = if statuses.is_empty() {
            vec!["active"]
        } else {
            statuses.to_vec()
        };
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
        let session_clause = if session_id.is_some() {
            " AND type = 'session' AND session_id = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
             WHERE project_path = ? AND status IN ({placeholders}){session_clause}
             ORDER BY updated_at_ms DESC, id DESC LIMIT ? OFFSET ?"
        );
        self.inner
            .with_conn(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut values: Vec<SqlValue> = Vec::with_capacity(statuses.len() + 4);
                values.push(SqlValue::Text(project_path.to_string()));
                for status in &statuses {
                    values.push(SqlValue::Text((*status).to_string()));
                }
                if let Some(session_id) = session_id {
                    values.push(SqlValue::Text(session_id.to_string()));
                }
                values.push(SqlValue::Integer(limit));
                values.push(SqlValue::Integer(offset));
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(values), stored_note_from_row)?
                    .collect::<Result<Vec<_>, _>>();
                rows
            })
            .map_err(Into::into)
    }

    pub fn read_smart_notes(
        &self,
        project_path: &str,
        statuses: &[&str],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredNote>, McStoreError> {
        let limit = i64::try_from(limit.clamp(1, 1000))
            .map_err(|_| McStoreError::Serde("note limit exceeds i64".to_string()))?;
        let offset = i64::try_from(offset)
            .map_err(|_| McStoreError::Serde("note offset exceeds i64".to_string()))?;
        let statuses = if statuses.is_empty() {
            vec!["active"]
        } else {
            statuses.to_vec()
        };
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
             WHERE project_path = ? AND type = 'smart' AND status IN ({placeholders})
             ORDER BY updated_at_ms DESC, id DESC LIMIT ? OFFSET ?"
        );
        self.inner
            .with_conn(|conn| {
                let mut statement = conn.prepare(&sql)?;
                let mut values: Vec<SqlValue> = Vec::with_capacity(statuses.len() + 3);
                values.push(SqlValue::Text(project_path.to_string()));
                for status in statuses {
                    values.push(SqlValue::Text(status.to_string()));
                }
                values.push(SqlValue::Integer(limit));
                values.push(SqlValue::Integer(offset));
                let rows = statement
                    .query_map(rusqlite::params_from_iter(values), stored_note_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .map_err(Into::into)
    }

    pub fn count_notes_by_type(
        &self,
        project_path: &str,
        type_name: &str,
        session_id: Option<&str>,
        statuses: &[&str],
    ) -> Result<usize, McStoreError> {
        if !matches!(type_name, "session" | "smart") {
            return Err(McStoreError::Serde("invalid note type".to_string()));
        }
        let statuses = if statuses.is_empty() {
            vec!["active"]
        } else {
            statuses.to_vec()
        };
        let placeholders = std::iter::repeat_n("?", statuses.len())
            .collect::<Vec<_>>()
            .join(", ");
        let session_clause = if session_id.is_some() {
            " AND session_id = ?"
        } else {
            ""
        };
        let sql = format!(
            "SELECT COUNT(*) FROM mc_notes WHERE project_path = ? AND type = ? AND status IN ({placeholders}){session_clause}"
        );
        self.inner
            .with_conn(|conn| {
                let mut values: Vec<SqlValue> = Vec::with_capacity(statuses.len() + 3);
                values.push(SqlValue::Text(project_path.to_string()));
                values.push(SqlValue::Text(type_name.to_string()));
                for status in statuses {
                    values.push(SqlValue::Text(status.to_string()));
                }
                if let Some(session_id) = session_id {
                    values.push(SqlValue::Text(session_id.to_string()));
                }
                conn.query_row(&sql, rusqlite::params_from_iter(values), |row| {
                    row.get::<_, i64>(0)
                })
            })
            .map_err(Into::into)
            .and_then(|count| {
                usize::try_from(count)
                    .map_err(|_| McStoreError::Serde("note count exceeds usize".to_string()))
            })
    }

    pub fn update_note_content(
        &self,
        project_path: &str,
        session_id: &str,
        note_id: i64,
        content: &str,
        now_ms: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        let current = self.get_note_by_id(project_path, session_id, note_id)?;
        let current = current.filter(|note| {
            matches!(
                note.status.as_str(),
                "active" | "pending" | "ready" | "surfacing" | "surfaced"
            )
        });
        let Some(current) = current else {
            return Ok(None);
        };
        match self.update_note_cas(
            project_path,
            note_id,
            &current.status,
            current.status_version,
            Some(content),
            None,
            now_ms,
        )? {
            NoteCasOutcome::Applied(note) => Ok(Some(note)),
            NoteCasOutcome::Conflict { .. } => Ok(None),
        }
    }

    /// Update note content and/or condition only when both the status and revision still
    /// match the caller's snapshot. A changed condition clears compiled evaluation state.
    #[allow(clippy::too_many_arguments)]
    pub fn update_note_cas(
        &self,
        project_path: &str,
        note_id: i64,
        expected_status: &str,
        expected_version: i64,
        content: Option<&str>,
        surface_condition: Option<Option<&str>>,
        now_ms: i64,
    ) -> Result<NoteCasOutcome, McStoreError> {
        self.require_note_project(project_path, note_id)?;
        let result = self.with_note_conn_fenced(project_path, |tx| {
            let current = load_note_tx(tx, note_id).optional()?;
            let Some(current) = current else {
                return Ok(NoteCasOutcome::Conflict { current: None });
            };
            if current.project_path != project_path {
                return Ok(NoteCasOutcome::Conflict { current: Some(current) });
            }
            if current.status != expected_status || current.status_version != expected_version {
                return Ok(NoteCasOutcome::Conflict { current: Some(current) });
            }
            let next_content = content.map(str::trim).unwrap_or(&current.content);
            if next_content.is_empty() {
                return Ok(NoteCasOutcome::Conflict { current: Some(current) });
            }
            let condition_changed = surface_condition.is_some();
            let next_condition = surface_condition
                .flatten()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let next_status = if condition_changed && next_condition.is_some() {
                "pending"
            } else {
                current.status.as_str()
            };
            let changed = tx.execute(
                "UPDATE mc_notes SET content = ?1, surface_condition = CASE WHEN ?2 THEN ?3 ELSE surface_condition END,
                    status = ?4, status_version = status_version + 1, updated_at_ms = ?5,
                    last_checked_at = CASE WHEN ?2 THEN NULL ELSE last_checked_at END,
                    ready_at = CASE WHEN ?2 THEN NULL ELSE ready_at END,
                    ready_reason = CASE WHEN ?2 THEN NULL ELSE ready_reason END,
                    compiled_check = CASE WHEN ?2 THEN NULL ELSE compiled_check END,
                    manifest_json = CASE WHEN ?2 THEN NULL ELSE manifest_json END,
                    check_hash = CASE WHEN ?2 THEN NULL ELSE check_hash END,
                    check_status = CASE WHEN ?2 THEN 'uncompiled' ELSE check_status END
                  WHERE id = ?6 AND project_path = ?7 AND status = ?8 AND status_version = ?9",
                params![
                    next_content,
                    condition_changed,
                    next_condition,
                    next_status,
                    now_ms,
                    note_id,
                    project_path,
                    expected_status,
                    expected_version,
                ],
            )?;
            if changed == 0 {
                return Ok(NoteCasOutcome::Conflict { current: load_note_tx(tx, note_id).optional()? });
            }
            Ok(NoteCasOutcome::Applied(load_note_tx(tx, note_id)?))
        })?;
        Ok(result)
    }

    pub fn dismiss_note(
        &self,
        project_path: &str,
        session_id: &str,
        note_id: i64,
        resolution: Option<&str>,
        now_ms: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        if self
            .get_note_by_id(project_path, session_id, note_id)?
            .is_none()
        {
            return Ok(None);
        }
        let resolution = resolution.map(str::trim).filter(|value| !value.is_empty());
        self.with_note_conn_fenced(project_path, |tx| {
            let Some(current) = load_note_tx(tx, note_id).optional()? else {
                return Ok(None);
            };
            if current.project_path != project_path
                || !matches!(
                    current.status.as_str(),
                    "active" | "pending" | "ready" | "surfacing" | "surfaced"
                )
            {
                return Ok(None);
            }
            let content = resolution
                .map(|value| format!("{}\n\nResolution: {value}", current.content))
                .unwrap_or_else(|| current.content.clone());
            let changed = tx.execute(
                "UPDATE mc_notes
                    SET status = 'dismissed', content = ?1,
                        status_version = status_version + 1, updated_at_ms = ?2,
                        dismissed_at = ?2, dismissal_resolution = ?3
                  WHERE id = ?4 AND project_path = ?5
                    AND status = ?6 AND status_version = ?7",
                params![
                    content,
                    now_ms,
                    resolution,
                    note_id,
                    project_path,
                    current.status,
                    current.status_version,
                ],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(Some(load_note_tx(tx, note_id)?))
        })
    }

    /// Dismissal is a status CAS. It wins over a later surfacing claim, but never rewrites
    /// bytes that a previous claim already placed in a frozen transform response.
    pub fn dismiss_note_cas(
        &self,
        project_path: &str,
        note_id: i64,
        expected_status: &str,
        expected_version: i64,
        resolution: Option<&str>,
        now_ms: i64,
    ) -> Result<NoteCasOutcome, McStoreError> {
        self.require_note_project(project_path, note_id)?;
        let outcome = self.transition_note_internal(
            project_path,
            note_id,
            expected_status,
            expected_version,
            "dismissed",
            resolution,
            now_ms,
        )?;
        if let NoteCasOutcome::Conflict {
            current: Some(current),
        } = &outcome
        {
            if current.status != "dismissed" {
                // Dismissal is the terminal user decision. Re-read the winner and apply
                // one fresh CAS so a concurrent surface/update cannot resurrect the note.
                return self.transition_note_internal(
                    project_path,
                    note_id,
                    &current.status,
                    current.status_version,
                    "dismissed",
                    resolution,
                    now_ms,
                );
            }
        }
        Ok(outcome)
    }

    /// Store a host-evaluated smart-note verdict under the evaluator's source revision.
    /// The module never interprets condition text; it only performs this CAS write and
    /// promotes a true verdict to `ready` for a later natural cache bust.
    pub fn write_note_evaluation(
        &self,
        input: NoteEvaluationInput<'_>,
    ) -> Result<NoteCasOutcome, McStoreError> {
        self.require_note_project(input.project_path, input.note_id)?;
        self.with_note_conn_fenced(input.project_path, |tx| {
            let Some(current) = load_note_tx(tx, input.note_id).optional()? else {
                return Ok(NoteCasOutcome::Conflict { current: None });
            };
            if current.project_path != input.project_path
                || current.status != "pending"
                || current.status_version != input.source_revision
            {
                return Ok(NoteCasOutcome::Conflict { current: Some(current) });
            }
            let status = if input.verdict { "ready" } else { "pending" };
            tx.execute(
                "UPDATE mc_notes SET status = ?1, status_version = status_version + 1,
                    updated_at_ms = ?2, ready_at = CASE WHEN ?1 = 'ready' THEN ?2 ELSE ready_at END,
                    ready_reason = CASE WHEN ?1 = 'ready' THEN 'condition_true' ELSE ready_reason END,
                    compiled_check = ?3, manifest_json = ?4, check_hash = ?5,
                    check_next_due_at = ?6, last_checked_at = ?2,
                    check_status = CASE WHEN ?1 = 'ready' THEN 'true' ELSE 'false' END
                  WHERE id = ?7 AND project_path = ?8 AND status = 'pending' AND status_version = ?9",
                params![
                    status,
                    input.now_ms,
                    input.compiled_check,
                    input.manifest_json,
                    input.check_hash,
                    input.next_due_at,
                    input.note_id,
                    input.project_path,
                    input.source_revision,
                ],
            )?;
            Ok(NoteCasOutcome::Applied(load_note_tx(tx, input.note_id)?))
        })
    }

    pub fn transition_note(
        &self,
        input: NoteTransitionInput<'_>,
    ) -> Result<NoteCasOutcome, McStoreError> {
        self.transition_note_internal(
            input.project_path,
            input.note_id,
            input.from_status,
            input.source_revision,
            input.to_status,
            input.result,
            input.now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn transition_note_internal(
        &self,
        project_path: &str,
        note_id: i64,
        expected_status: &str,
        expected_version: i64,
        to_status: &str,
        result: Option<&str>,
        now_ms: i64,
    ) -> Result<NoteCasOutcome, McStoreError> {
        self.require_note_project(project_path, note_id)?;
        if !matches!(
            to_status,
            "active" | "pending" | "ready" | "surfacing" | "surfaced" | "dismissed"
        ) {
            return Err(McStoreError::Serde(format!(
                "invalid note transition target {to_status}"
            )));
        }
        self.with_note_conn_fenced(project_path, |tx| {
            let Some(current) = load_note_tx(tx, note_id).optional()? else {
                return Ok(NoteCasOutcome::Conflict { current: None });
            };
            if current.project_path != project_path
                || current.status != expected_status
                || current.status_version != expected_version
            {
                return Ok(NoteCasOutcome::Conflict { current: Some(current) });
            }
            tx.execute(
                "UPDATE mc_notes SET status = ?1, status_version = status_version + 1,
                    updated_at_ms = ?2,
                    ready_at = CASE WHEN ?1 = 'ready' THEN COALESCE(ready_at, ?2) ELSE ready_at END,
                    ready_reason = CASE WHEN ?1 = 'ready' THEN COALESCE(?3, ready_reason) ELSE ready_reason END,
                    dismissed_at = CASE WHEN ?1 = 'dismissed' THEN ?2 ELSE dismissed_at END,
                    dismissal_resolution = CASE WHEN ?1 = 'dismissed' THEN ?3 ELSE dismissal_resolution END
                  WHERE id = ?4 AND project_path = ?5 AND status = ?6 AND status_version = ?7",
                params![to_status, now_ms, result, note_id, project_path, expected_status, expected_version],
            )?;
            Ok(NoteCasOutcome::Applied(load_note_tx(tx, note_id)?))
        })
    }

    /// Claim one due pending smart note for an evaluator. The source revision is the
    /// status_version observed by the evaluator; the lease itself is represented by the
    /// module-internal `surfacing` state and is released by a CAS transition.
    pub fn claim_due_note(
        &self,
        project_path: &str,
        now_ms: i64,
    ) -> Result<Option<StoredNote>, McStoreError> {
        self.with_note_conn_fenced(project_path, |tx| {
            let note = tx
                .query_row(
                    &format!(
                        "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
                         WHERE project_path = ?1 AND type = 'smart' AND status = 'pending'
                           AND (check_next_due_at IS NULL OR check_next_due_at <= ?2)
                           AND (check_quarantined_until IS NULL OR check_quarantined_until <= ?2)
                         ORDER BY COALESCE(check_next_due_at, 0), id LIMIT 1"
                    ),
                    params![project_path, now_ms],
                    stored_note_from_row,
                )
                .optional()?;
            let Some(note) = note else { return Ok(None) };
            let changed = tx.execute(
                "UPDATE mc_notes SET status_version = status_version + 1, updated_at_ms = ?1
                   WHERE id = ?2 AND project_path = ?3 AND status = 'pending' AND status_version = ?4",
                params![now_ms, note.id, project_path, note.status_version],
            )?;
            if changed == 0 { return Ok(None); }
            tx.query_row(
                &format!("SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes WHERE id = ?1"),
                params![note.id],
                stored_note_from_row,
            ).map(Some)
        })
    }

    pub fn claim_note_delivery(
        &self,
        project_path: &str,
        session_id: &str,
        delivered_pass_fingerprint: &str,
        transform_pass_id: &str,
        now_ms: i64,
    ) -> Result<Vec<(StoredNote, NoteDelivery)>, McStoreError> {
        self.with_note_conn_fenced(project_path, |tx| {
            let notes = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes
                     WHERE project_path = ?1 AND type = 'smart' AND
                       (status = 'ready' OR status = 'surfacing' OR status = 'surfaced')
                     ORDER BY updated_at_ms ASC, id ASC"
                ))?;
                let rows = stmt
                    .query_map(params![project_path], stored_note_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let mut candidates = Vec::new();
            for note in notes {
                let unacked = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM mc_note_deliveries
                       WHERE project_path = ?1 AND note_id = ?2 AND session_id = ?3 AND disposition IS NULL)",
                    params![project_path, note.id, session_id],
                    |r| r.get::<_, i64>(0),
                )? != 0;
                if note.status == "surfaced" && !unacked {
                    continue;
                }
                let existing = tx
                    .query_row(
                        "SELECT delivery_id, transform_pass_id, acked_at
                           FROM mc_note_deliveries
                          WHERE project_path = ?1 AND note_id = ?2 AND session_id = ?3
                            AND delivered_pass_fingerprint = ?4 AND disposition IS NULL",
                        params![project_path, note.id, session_id, delivered_pass_fingerprint],
                        |r| {
                            Ok(NoteDelivery {
                                delivery_id: r.get(0)?,
                                note_id: note.id,
                                session_id: session_id.to_string(),
                                delivered_pass_fingerprint: delivered_pass_fingerprint.to_string(),
                                transform_pass_id: r.get(1)?,
                                acked_at: r.get(2)?,
                            })
                        },
                    )
                    .optional()?;
                if let Some(delivery) = existing {
                    if delivery.acked_at.is_none() {
                        candidates.push((note, delivery));
                    }
                    continue;
                }
                if note.status == "ready" {
                    let changed = tx.execute(
                        "UPDATE mc_notes SET status = 'surfacing', status_version = status_version + 1,
                            updated_at_ms = ?1
                          WHERE id = ?2 AND project_path = ?3 AND status = 'ready' AND status_version = ?4",
                        params![now_ms, note.id, project_path, note.status_version],
                    )?;
                    if changed == 0 {
                        continue;
                    }
                }
                let delivery_id = format!(
                    "{}:{project_path}:{}:{session_id}:{transform_pass_id}:{}",
                    project_path.len(),
                    session_id.len(),
                    note.id
                );
                tx.execute(
                    "INSERT INTO mc_note_deliveries
                       (delivery_id, note_id, session_id, delivered_pass_fingerprint,
                        transform_pass_id, created_at_ms, project_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        delivery_id,
                        note.id,
                        session_id,
                        delivered_pass_fingerprint,
                        transform_pass_id,
                        now_ms,
                        project_path
                    ],
                )?;
                let stored = load_note_tx(tx, note.id)?;
                candidates.push((
                    stored,
                    NoteDelivery {
                        delivery_id,
                        note_id: note.id,
                        session_id: session_id.to_string(),
                        delivered_pass_fingerprint: delivered_pass_fingerprint.to_string(),
                        transform_pass_id: transform_pass_id.to_string(),
                        acked_at: None,
                    },
                ));
            }
            Ok(candidates)
        })
    }

    pub fn ack_note_delivery(
        &self,
        project_path: &str,
        session_id: &str,
        transform_pass_id: &str,
        now_ms: i64,
    ) -> Result<usize, McStoreError> {
        self.with_note_conn_fenced(project_path, |tx| {
            let ids = tx
                .prepare(
                    "SELECT DISTINCT note_id FROM mc_note_deliveries
                       WHERE project_path = ?1 AND session_id = ?2 AND transform_pass_id = ?3
                         AND disposition IS NULL",
                )?
                .query_map(
                    params![project_path, session_id, transform_pass_id],
                    |row| row.get::<_, i64>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let changed = tx.execute(
                "UPDATE mc_note_deliveries SET acked_at = ?1, disposition = 'acked'
                   WHERE project_path = ?2 AND session_id = ?3 AND transform_pass_id = ?4
                     AND disposition IS NULL",
                params![now_ms, project_path, session_id, transform_pass_id],
            )?;
            for id in ids {
                tx.execute(
                    "UPDATE mc_note_deliveries SET disposition = 'superseded'
                       WHERE project_path = ?1 AND note_id = ?2 AND session_id = ?3
                         AND disposition IS NULL",
                    params![project_path, id, session_id],
                )?;
                tx.execute(
                    "UPDATE mc_notes SET status = 'surfaced', status_version = status_version + 1,
                        updated_at_ms = ?1
                      WHERE id = ?2 AND project_path = ?3 AND status IN ('surfacing', 'surfaced')",
                    params![now_ms, id, project_path],
                )?;
            }
            Ok(changed)
        })
    }

    pub fn nack_note_delivery(
        &self,
        project_path: &str,
        session_id: &str,
        transform_pass_id: &str,
        now_ms: i64,
    ) -> Result<usize, McStoreError> {
        self.with_note_conn_fenced(project_path, |tx| {
            let ids = tx
                .prepare(
                    "SELECT DISTINCT note_id FROM mc_note_deliveries
                       WHERE project_path = ?1 AND session_id = ?2 AND transform_pass_id = ?3
                         AND disposition IS NULL",
                )?
                .query_map(params![project_path, session_id, transform_pass_id], |r| {
                    r.get::<_, i64>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let changed = tx.execute(
                "UPDATE mc_note_deliveries SET disposition = 'nacked'
                   WHERE project_path = ?1 AND session_id = ?2 AND transform_pass_id = ?3
                     AND disposition IS NULL",
                params![project_path, session_id, transform_pass_id],
            )?;
            for id in &ids {
                tx.execute(
                    "UPDATE mc_notes SET status = 'ready', status_version = status_version + 1,
                        updated_at_ms = ?1
                      WHERE id = ?2 AND project_path = ?3 AND status IN ('surfacing', 'surfaced')",
                    params![now_ms, id, project_path],
                )?;
            }
            Ok(changed)
        })
    }

    pub fn search_notes_like(
        &self,
        project_path: &str,
        _session_id: &str,
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
                   WHERE project_path = ?1
                     AND (LOWER(content) LIKE ?2 ESCAPE '\\'
                       OR LOWER(COALESCE(surface_condition, '')) LIKE ?2 ESCAPE '\\')
                  ORDER BY updated_at_ms DESC, id DESC
                  LIMIT 100",
            )?;
            let mapped = stmt
                .query_map(params![project_path, pattern], |r| {
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

    /// Load the profile lines mirrored for one shadow project. Shadow composition must not
    /// read the global user-memory table owned by the production leg.
    pub fn load_shadow_user_profile(
        &self,
        shadow_project_path: &str,
    ) -> Result<Vec<String>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content FROM shadow_user_profile
                 WHERE shadow_project_path = ?1
                 ORDER BY profile_index ASC",
            )?;
            let mapped = stmt
                .query_map(params![shadow_project_path], |row| row.get::<_, String>(0))?
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
    /// Mark a seeded memory shareable with a workspace-eligible scope. Foreign
    /// visibility is fail-closed (rows default to unshareable until classification),
    /// so cross-project read tests must opt rows in explicitly.
    pub fn set_memory_sharing_for_test(
        &self,
        id: i64,
        scope: &str,
        shareable: bool,
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "UPDATE mc_memories SET scope = ?2, shareable = ?3 WHERE id = ?1",
                params![id, scope, shareable as i64],
            )?;
            Ok(())
        })?;
        Ok(())
    }

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

impl McStore {
    /// Read the durable authority row. A missing row is normal for a store that has
    /// never opted a project into module ownership.
    pub fn authority_status(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
    ) -> Result<Option<AuthorityRow>, McStoreError> {
        self.inner
            .with_conn(|conn| {
                conn.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
                .optional()
            })
            .map_err(Into::into)
    }

    /// Enter PREPARING and bump the generation. Repeating the request is idempotent
    /// only when the row is already preparing at the same generation.
    pub fn authority_begin_prepare(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        self.with_note_conn_fenced(project, |tx| {
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES (?1, ?2, ?3, 'TS') ON CONFLICT(context_store_uuid, project, domain) DO NOTHING",
                    params![context_store_uuid, project, domain],
                )?;
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.state == "TS" {
                    if domain == "memories" {
                        tx.execute(
                            "DELETE FROM mc_memories WHERE context_store_uuid = ?1 AND project_path = ?2",
                            params![context_store_uuid, project],
                        )?;
                        tx.execute(
                            "DELETE FROM mc_authority_pending_memory_references
                              WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3",
                            params![context_store_uuid, project, domain],
                        )?;
                    } else {
                        tx.execute(
                            "DELETE FROM mc_notes WHERE context_store_uuid = ?1 AND project_path = ?2",
                            params![context_store_uuid, project],
                        )?;
                    }
                    tx.execute(
                        "DELETE FROM mc_authority_seed_rows WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3",
                        params![context_store_uuid, project, domain],
                    )?;
                    tx.execute(
                        "UPDATE mc_authority SET state = 'PREPARING', generation = generation + 1,
                                checksum_expected = NULL, checksum_actual = NULL, checksum_ok = NULL
                         WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3",
                        params![context_store_uuid, project, domain],
                    )?;
                } else if current.state != "PREPARING" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "TS".to_string(),
                            found: current.state,
                        },
                    )));
                }
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn authority_verify_prepare(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
        checksum_expected: &str,
        checksum_actual: &str,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.generation != expected_generation || current.state != "PREPARING" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: format!("PREPARING generation {expected_generation}"),
                            found: format!("{} generation {}", current.state, current.generation),
                        },
                    )));
                }
                if domain == "memories" {
                    let pending: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM mc_authority_pending_memory_references
                          WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3",
                        params![context_store_uuid, project, domain],
                        |row| row.get(0),
                    )?;
                    if pending > 0 {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                            AuthorityTransitionError::UnresolvedPendingReferences { count: pending },
                        )));
                    }
                }
                tx.execute(
                    "UPDATE mc_authority SET checksum_expected = ?1, checksum_actual = ?2, checksum_ok = ?3
                       WHERE context_store_uuid = ?4 AND project = ?5 AND domain = ?6",
                    params![
                        checksum_expected,
                        checksum_actual,
                        if checksum_expected == checksum_actual { 1 } else { 0 },
                        context_store_uuid,
                        project,
                        domain
                    ],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
            .map_err(map_authority_sql_error)
    }

    pub fn authority_ack_prepare(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.generation != expected_generation
                    || current.state != "PREPARING"
                    || current.checksum_ok != Some(true)
                {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: format!(
                                "verified PREPARING generation {expected_generation}"
                            ),
                            found: format!("{} generation {}", current.state, current.generation),
                        },
                    )));
                }
                tx.execute(
                    "UPDATE mc_authority SET state = 'MODULE', generation = generation + 1
                       WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3",
                    params![context_store_uuid, project, domain],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
            .map_err(map_authority_sql_error)
    }

    /// Record seed verification and complete TS -> MODULE. The host keeps its
    /// context.db barrier until this transition has committed on the module side.
    /// The arguments are kept explicit because each value is part of the durable
    /// transition record and must be validated together.
    #[allow(clippy::too_many_arguments)]
    pub fn authority_finish_prepare(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
        checksum_expected: &str,
        checksum_actual: &str,
        verified: bool,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.generation != expected_generation {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::Generation {
                            expected: expected_generation,
                            found: current.generation,
                        },
                    )));
                }
                if current.state != "PREPARING" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "PREPARING".to_string(),
                            found: current.state,
                        },
                    )));
                }
                if verified && checksum_expected != checksum_actual {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "matching checksums".to_string(),
                            found: "verification failed".to_string(),
                        },
                    )));
                }
                let next_state = if verified { "MODULE" } else { "TS" };
                tx.execute(
                    "UPDATE mc_authority
                        SET state = ?1, generation = generation + 1,
                            checksum_expected = ?2, checksum_actual = ?3, checksum_ok = ?4
                      WHERE context_store_uuid = ?5 AND project = ?6 AND domain = ?7",
                    params![
                        next_state,
                        checksum_expected,
                        checksum_actual,
                        if verified { 1 } else { 0 },
                        context_store_uuid,
                        project,
                        domain
                    ],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
            .map_err(map_authority_sql_error)
    }

    /// Abort a failed preparation without erasing the diagnostic checksum.
    pub fn authority_abort_prepare(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
    ) -> Result<AuthorityRow, McStoreError> {
        self.authority_finish_prepare(
            context_store_uuid,
            project,
            domain,
            expected_generation,
            "",
            "",
            false,
        )
    }

    pub fn authority_begin_drain(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        lease: &str,
        lease_expires_at: i64,
        now_ms: i64,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        self.inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.state == "DRAINING" {
                    let held_by_other = current.coordinator_lease.as_deref() != Some(lease);
                    let lease_live = current.lease_expires_at.unwrap_or(0) > now_ms;
                    if held_by_other && lease_live {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                            AuthorityTransitionError::State {
                                expected: "the existing drain coordinator or an expired lease"
                                    .to_string(),
                                found: "a different live drain coordinator".to_string(),
                            },
                        )));
                    }
                    // Takeover or same-lease resume mints a fresh token so the prior
                    // coordinator cannot keep journaling with a stale attempt identity. It also
                    // captures a new feed head after a finish fence reports a late append.
                    let token = mint_coordinator_token(lease, lease_expires_at, current.generation);
                    let feed_head: i64 = tx.query_row(
                        "SELECT COALESCE(MAX(feed_seq), 0) FROM mc_changefeed WHERE domain = ?1",
                        params![domain],
                        |row| row.get(0),
                    )?;
                    let captured_upper_bound = current
                        .captured_upper_bound
                        .unwrap_or(0)
                        .max(feed_head);
                    tx.execute(
                        "UPDATE mc_authority
                            SET coordinator_lease = ?1, lease_expires_at = ?2, coordinator_token = ?3,
                                captured_upper_bound = ?4
                          WHERE context_store_uuid = ?5 AND project = ?6 AND domain = ?7",
                        params![
                            lease,
                            lease_expires_at,
                            token,
                            captured_upper_bound,
                            context_store_uuid,
                            project,
                            domain
                        ],
                    )?;
                    return tx.query_row(
                        AUTHORITY_SELECT_SQL,
                        params![context_store_uuid, project, domain],
                        authority_row_from_sql,
                    );
                }
                if current.state != "MODULE" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "MODULE".to_string(),
                            found: current.state,
                        },
                    )));
                }
                let upper_bound: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(feed_seq), 0) FROM mc_changefeed WHERE domain = ?1",
                    params![domain],
                    |row| row.get(0),
                )?;
                let next_generation = current.generation + 1;
                let token = mint_coordinator_token(lease, lease_expires_at, next_generation);
                tx.execute(
                    "UPDATE mc_authority
                        SET state = 'DRAINING', generation = generation + 1,
                            captured_upper_bound = ?1, drain_generation = generation + 1,
                            drain_cursor = 0, coordinator_lease = ?2, lease_expires_at = ?3,
                            coordinator_token = ?4,
                            step_seed = 0, step_memories = 0, step_notes = 0,
                            step_compartments = 0, step_reconcile = 0, step_verify = 0, step_flip = 0
                      WHERE context_store_uuid = ?5 AND project = ?6 AND domain = ?7",
                    params![
                        upper_bound,
                        lease,
                        lease_expires_at,
                        token,
                        context_store_uuid,
                        project,
                        domain
                    ],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
            .map_err(map_authority_sql_error)
    }

    /// Advance one idempotent DRAINING journal bit or its feed cursor.
    #[allow(clippy::too_many_arguments)]
    pub fn authority_drain_step(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
        step: &str,
        cursor: Option<i64>,
        coordinator_token: &str,
        now_ms: i64,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        let column = match step {
            "seed" => "step_seed",
            "memories" => "step_memories",
            "notes" => "step_notes",
            "compartments" => "step_compartments",
            "reconcile" => "step_reconcile",
            "verify" => "step_verify",
            "flip" => "step_flip",
            _ => return Err(McStoreError::Serde(format!("unknown drain step {step}"))),
        };
        self.inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.generation != expected_generation {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::Generation {
                            expected: expected_generation,
                            found: current.generation,
                        },
                    )));
                }
                if current.state != "DRAINING" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "DRAINING".to_string(),
                            found: current.state,
                        },
                    )));
                }
                authority_require_live_coordinator(&current, coordinator_token, now_ms)?;
                tx.execute(
                    &format!("UPDATE mc_authority SET {column} = 1, drain_cursor = COALESCE(?1, drain_cursor) WHERE context_store_uuid = ?2 AND project = ?3 AND domain = ?4"),
                    params![cursor, context_store_uuid, project, domain],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
            })
            .map_err(map_authority_sql_error)
    }

    /// Complete DRAINING after the host has verified every journal step.
    /// The checksum, generation, coordinator token, and captured feed head are checked
    /// together so a stale coordinator cannot publish a partial handoff.
    #[allow(clippy::too_many_arguments)]
    pub fn authority_finish_drain(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
        expected_generation: u64,
        checksum_expected: &str,
        checksum_actual: &str,
        verified: bool,
        coordinator_token: &str,
        now_ms: i64,
    ) -> Result<AuthorityRow, McStoreError> {
        validate_authority_domain(domain)?;
        let outcome = self
            .inner
            .with_conn_fenced(|tx| {
                let current = tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )?;
                if current.generation != expected_generation {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::Generation {
                            expected: expected_generation,
                            found: current.generation,
                        },
                    )));
                }
                if current.state != "DRAINING" {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "DRAINING".to_string(),
                            found: current.state,
                        },
                    )));
                }
                authority_require_live_coordinator(&current, coordinator_token, now_ms)?;
                let all_steps = current.step_seed
                    && current.step_memories
                    && current.step_notes
                    && current.step_compartments
                    && current.step_reconcile
                    && current.step_verify;
                if !all_steps || !verified || checksum_expected != checksum_actual {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        AuthorityTransitionError::State {
                            expected: "all drain steps and verification".to_string(),
                            found: "incomplete or failed".to_string(),
                        },
                    )));
                }
                let feed_head: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(feed_seq), 0) FROM mc_changefeed WHERE domain = ?1",
                    params![domain],
                    |row| row.get(0),
                )?;
                let captured = current.captured_upper_bound.unwrap_or(0);
                if feed_head != captured {
                    // This comparison shares the transaction with the ownership flip. A writer
                    // either commits before the flip and forces replay, or after TypeScript owns the domain.
                    return Ok(AuthorityFinishDrainOutcome::FeedHeadAdvanced {
                        captured,
                        found: feed_head,
                    });
                }
                tx.execute(
                    "UPDATE mc_authority SET state = 'TS', generation = generation + 1,
                            checksum_expected = ?1, checksum_actual = ?2, checksum_ok = ?3,
                            coordinator_lease = NULL, lease_expires_at = NULL,
                            coordinator_token = NULL, step_flip = 1
                      WHERE context_store_uuid = ?4 AND project = ?5 AND domain = ?6",
                    params![
                        checksum_expected,
                        checksum_actual,
                        if verified { 1 } else { 0 },
                        context_store_uuid,
                        project,
                        domain
                    ],
                )?;
                tx.query_row(
                    AUTHORITY_SELECT_SQL,
                    params![context_store_uuid, project, domain],
                    authority_row_from_sql,
                )
                .map(Box::new)
                .map(AuthorityFinishDrainOutcome::Finished)
            })
            .map_err(map_authority_sql_error)?;
        match outcome {
            AuthorityFinishDrainOutcome::Finished(row) => Ok(*row),
            AuthorityFinishDrainOutcome::FeedHeadAdvanced { captured, found } => {
                Err(McStoreError::AuthorityFeedHeadAdvanced { captured, found })
            }
        }
    }

    /// Upsert a seeded context row by its durable source key. The source key is
    /// intentionally independent of the module's integer id allocation.
    pub fn seed_authority_row(
        &self,
        context_store_uuid: &str,
        domain: &str,
        source_row_id: i64,
        snapshot: &Value,
    ) -> Result<i64, McStoreError> {
        validate_authority_domain(domain)?;
        match domain {
            "memories" => self.seed_memory_snapshot(context_store_uuid, source_row_id, snapshot),
            "notes" => self.seed_note_snapshot(context_store_uuid, source_row_id, snapshot),
            _ => unreachable!(),
        }
    }

    pub fn authority_seed_checksum(
        &self,
        context_store_uuid: &str,
        project: &str,
        domain: &str,
    ) -> Result<String, McStoreError> {
        validate_authority_domain(domain)?;
        let rows = self.inner.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT source_row_id, snapshot_json FROM mc_authority_seed_rows WHERE context_store_uuid = ?1 AND project = ?2 AND domain = ?3 ORDER BY source_row_id ASC",
            )?;
            let rows = statement
                .query_map(params![context_store_uuid, project, domain], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut canonical_rows = Vec::with_capacity(rows.len());
        for (source_row_id, snapshot_json) in rows {
            let snapshot: Value = serde_json::from_str(&snapshot_json)
                .map_err(|error| McStoreError::Serde(error.to_string()))?;
            canonical_rows.push(serde_json::json!({
                "source_row_id": source_row_id,
                "snapshot": snapshot,
            }));
        }
        let canonical = canonical_authority_value(&Value::Array(canonical_rows));
        Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
    }

    fn seed_memory_snapshot(
        &self,
        context_store_uuid: &str,
        source_row_id: i64,
        snapshot: &Value,
    ) -> Result<i64, McStoreError> {
        let object = snapshot.as_object().ok_or_else(|| {
            McStoreError::Serde("memory seed snapshot must be an object".to_string())
        })?;
        let text = |name: &str| object.get(name).and_then(Value::as_str);
        let integer = |name: &str| object.get(name).and_then(Value::as_i64);
        let project = text("project_path").unwrap_or_default().to_string();
        let snapshot_json = serde_json::to_string(snapshot)
            .map_err(|error| McStoreError::Serde(error.to_string()))?;
        let id = self.inner.with_conn_fenced(|tx| {
            let target_source_row_id = integer("superseded_by_memory_id");
            let superseded_by_memory_id = match target_source_row_id {
                Some(target_source_row_id) => tx
                    .query_row(
                        "SELECT id FROM mc_memories WHERE context_store_uuid = ?1 AND context_row_id = ?2",
                        params![context_store_uuid, target_source_row_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?,
                None => None,
            };
            tx.execute(
                "INSERT INTO mc_memories
                    (project_path, category, content, normalized_hash, importance, scope, shareable,
                     source_session_id, source_type, seen_count, retrieval_count, first_seen_at,
                     created_at, updated_at, last_seen_at, last_retrieved_at, status, expires_at,
                     verification_status, verified_at, classified_at, superseded_by_memory_id,
                     merged_from, metadata_json, context_store_uuid, context_row_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)
                 ON CONFLICT(context_store_uuid, context_row_id) DO UPDATE SET
                    project_path=excluded.project_path, category=excluded.category,
                    content=excluded.content, normalized_hash=excluded.normalized_hash,
                    importance=excluded.importance, scope=excluded.scope, shareable=excluded.shareable,
                    source_session_id=excluded.source_session_id, source_type=excluded.source_type,
                    seen_count=excluded.seen_count, retrieval_count=excluded.retrieval_count,
                    first_seen_at=excluded.first_seen_at, created_at=excluded.created_at,
                    updated_at=excluded.updated_at, last_seen_at=excluded.last_seen_at,
                    last_retrieved_at=excluded.last_retrieved_at, status=excluded.status,
                    expires_at=excluded.expires_at, verification_status=excluded.verification_status,
                    verified_at=excluded.verified_at, classified_at=excluded.classified_at,
                    superseded_by_memory_id=excluded.superseded_by_memory_id,
                    merged_from=excluded.merged_from, metadata_json=excluded.metadata_json",
                params![
                    project,
                    text("category").unwrap_or_default(),
                    text("content").unwrap_or_default(),
                    text("normalized_hash").unwrap_or_default(),
                    integer("importance"),
                    text("scope").unwrap_or("project"),
                    integer("shareable").unwrap_or(0),
                    text("source_session_id"),
                    text("source_type").unwrap_or("historian"),
                    integer("seen_count").unwrap_or(1),
                    integer("retrieval_count").unwrap_or(0),
                    integer("first_seen_at").unwrap_or(0),
                    integer("created_at").unwrap_or(0),
                    integer("updated_at").unwrap_or(0),
                    integer("last_seen_at").unwrap_or(0),
                    integer("last_retrieved_at"),
                    text("status").unwrap_or("active"),
                    integer("expires_at"),
                    text("verification_status").unwrap_or("unverified"),
                    integer("verified_at"),
                    integer("classified_at"),
                    superseded_by_memory_id,
                     text("merged_from"),
                    text("metadata_json"),
                    context_store_uuid,
                    source_row_id,
                ],
            )?;
            let id: i64 = tx.query_row(
                "SELECT id FROM mc_memories WHERE context_store_uuid = ?1 AND context_row_id = ?2",
                params![context_store_uuid, source_row_id],
                |row| row.get(0),
            )?;
            match (target_source_row_id, superseded_by_memory_id) {
                (Some(target), None) => {
                    tx.execute(
                        "INSERT INTO mc_authority_pending_memory_references(
                            context_store_uuid, project, domain, source_context_row_id, target_context_row_id
                         ) VALUES (?1, ?2, 'memories', ?3, ?4)
                         ON CONFLICT(context_store_uuid, project, domain, source_context_row_id)
                         DO UPDATE SET target_context_row_id = excluded.target_context_row_id",
                        params![context_store_uuid, project, source_row_id, target],
                    )?;
                }
                _ => {
                    tx.execute(
                        "DELETE FROM mc_authority_pending_memory_references
                          WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'
                            AND source_context_row_id = ?3",
                        params![context_store_uuid, project, source_row_id],
                    )?;
                }
            }
            let pending = {
                let mut statement = tx.prepare(
                    "SELECT source_context_row_id, target_context_row_id
                       FROM mc_authority_pending_memory_references
                      WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'",
                )?;
                let rows = statement
                    .query_map(params![context_store_uuid, project], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for (source, target) in pending {
                let translated = tx
                    .query_row(
                        "SELECT id FROM mc_memories
                          WHERE context_store_uuid = ?1 AND project_path = ?2 AND context_row_id = ?3",
                        params![context_store_uuid, project, target],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                let Some(translated) = translated else { continue };
                tx.execute(
                    "UPDATE mc_memories SET superseded_by_memory_id = ?1
                      WHERE context_store_uuid = ?2 AND project_path = ?3 AND context_row_id = ?4",
                    params![translated, context_store_uuid, project, source],
                )?;
                tx.execute(
                    "DELETE FROM mc_authority_pending_memory_references
                      WHERE context_store_uuid = ?1 AND project = ?2 AND domain = 'memories'
                        AND source_context_row_id = ?3",
                    params![context_store_uuid, project, source],
                )?;
            }
            if let Some(mapping) = object.get("mapping") {
                let mapped_files_json = serde_json::to_string(mapping)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.execute(
                    "INSERT INTO mc_memory_mappings(memory_id, project_path, mapped_files_json, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(memory_id) DO UPDATE SET project_path = excluded.project_path, mapped_files_json = excluded.mapped_files_json, updated_at = excluded.updated_at",
                    params![id, project, mapped_files_json, integer("updated_at").unwrap_or(0)],
                )?;
            } else {
                tx.execute("DELETE FROM mc_memory_mappings WHERE memory_id = ?1", params![id])?;
            }
            tx.execute(
                "INSERT INTO mc_authority_seed_rows(context_store_uuid, project, domain, source_row_id, snapshot_json) VALUES (?1, ?2, 'memories', ?3, ?4) ON CONFLICT(context_store_uuid, project, domain, source_row_id) DO UPDATE SET snapshot_json = excluded.snapshot_json",
                params![context_store_uuid, project, source_row_id, snapshot_json],
            )?;
            Ok(id)
        })?;
        Ok(id)
    }

    fn seed_note_snapshot(
        &self,
        context_store_uuid: &str,
        source_row_id: i64,
        snapshot: &Value,
    ) -> Result<i64, McStoreError> {
        let object = snapshot.as_object().ok_or_else(|| {
            McStoreError::Serde("note seed snapshot must be an object".to_string())
        })?;
        let text = |name: &str| object.get(name).and_then(Value::as_str);
        let integer = |name: &str| object.get(name).and_then(Value::as_i64);
        let project = text("project_path").unwrap_or_default().to_string();
        let snapshot_json = serde_json::to_string(snapshot)
            .map_err(|error| McStoreError::Serde(error.to_string()))?;
        let id = self.with_note_conn_fenced(&project, |tx| {
            tx.execute(
                &format!(
"INSERT INTO mc_notes ({NOTE_INSERT_COLUMNS}) VALUES (
                              ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                              ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                              ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)
                     ON CONFLICT(context_store_uuid, context_row_id) DO UPDATE SET
                        type=excluded.type, project_path=excluded.project_path,
                        session_id=excluded.session_id, content=excluded.content,
                        status=excluded.status, surface_condition=excluded.surface_condition,
                        ready_at=excluded.ready_at, ready_reason=excluded.ready_reason,
                        manifest_json=excluded.manifest_json, compiled_check=excluded.compiled_check,
                        check_hash=excluded.check_hash, check_cron=excluded.check_cron,
                        check_failure_count=excluded.check_failure_count,
                        check_network_failure_count=excluded.check_network_failure_count,
                        check_quarantined_until=excluded.check_quarantined_until,
                        check_next_due_at=excluded.check_next_due_at, check_compiled_at=excluded.check_compiled_at,
                        check_false_since_at=excluded.check_false_since_at,
                        check_last_liveness_at=excluded.check_last_liveness_at,
                        last_checked_at=excluded.last_checked_at, check_status=excluded.check_status,
                        check_version=excluded.check_version, policy_version=excluded.policy_version,
                        harness=excluded.harness, anchor_block_id=excluded.anchor_block_id,
                        anchor_ordinal=excluded.anchor_ordinal, dismissed_at=excluded.dismissed_at,
                        dismissal_resolution=excluded.dismissal_resolution,
                        status_version=excluded.status_version, created_at_ms=excluded.created_at_ms,
                        updated_at_ms=excluded.updated_at_ms"
                ),
                params![
                          text("type").unwrap_or("smart"),
                    project,
                          text("session_id"),
                    text("content").unwrap_or(""),
                    text("status").unwrap_or("active"),
                    text("surface_condition"),
                    integer("ready_at"),
                    text("ready_reason"),
                    text("manifest_json"),
                    text("compiled_check"),
                    text("check_hash"),
                    text("check_cron"),
                    integer("check_failure_count").unwrap_or(0),
                    integer("check_network_failure_count").unwrap_or(0),
                    integer("check_quarantined_until"),
                    integer("check_next_due_at"),
                    integer("check_compiled_at"),
                    integer("check_false_since_at"),
                    integer("check_last_liveness_at"),
                    integer("last_checked_at"),
                          text("check_status").unwrap_or("uncompiled"),
                          integer("check_version").unwrap_or(0),
                          integer("policy_version").unwrap_or(1),
                    text("harness").unwrap_or("module"),
                    text("anchor_block_id"),
                    integer("anchor_ordinal"),
                    integer("dismissed_at"),
                    text("dismissal_resolution"),
                    integer("status_version").unwrap_or(0),
                          integer("created_at_ms").or_else(|| integer("created_at")).unwrap_or(0),
                          integer("updated_at_ms").or_else(|| integer("updated_at")).unwrap_or(0),
                    context_store_uuid,
                    source_row_id,
                ],
            )?;
            let id = tx.query_row(
                "SELECT id FROM mc_notes WHERE context_store_uuid = ?1 AND context_row_id = ?2",
                params![context_store_uuid, source_row_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO mc_authority_seed_rows(context_store_uuid, project, domain, source_row_id, snapshot_json) VALUES (?1, ?2, 'notes', ?3, ?4) ON CONFLICT(context_store_uuid, project, domain, source_row_id) DO UPDATE SET snapshot_json = excluded.snapshot_json",
                params![context_store_uuid, project, source_row_id, snapshot_json],
            )?;
            Ok(id)
        })?;
        Ok(id)
    }

    /// Pull a bounded, ordered feed page. The cursor is a global feed sequence;
    /// filtering by domain preserves monotonic retry semantics even when domains interleave.
    pub fn pull_changefeed(
        &self,
        domain: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<ChangefeedPage, McStoreError> {
        validate_authority_domain(domain)?;
        let limit = i64::try_from(limit.clamp(1, 1000))
            .map_err(|_| McStoreError::Serde("feed limit exceeds i64".to_string()))?;
        self.inner
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT feed_seq, domain, op, module_row_id, full_row_snapshot, content_hash
                       FROM mc_changefeed WHERE domain = ?1 AND feed_seq > ?2
                       ORDER BY feed_seq ASC LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(params![domain, cursor, limit], |row| {
                        let snapshot: String = row.get(4)?;
                        Ok(ChangefeedRow {
                            feed_seq: row.get(0)?,
                            domain: row.get(1)?,
                            op: row.get(2)?,
                            module_row_id: row.get(3)?,
                            full_row_snapshot: serde_json::from_str(&snapshot).map_err(
                                |error| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        4,
                                        rusqlite::types::Type::Text,
                                        Box::new(error),
                                    )
                                },
                            )?,
                            content_hash: row.get(5)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let next_cursor = rows.last().map(|row| row.feed_seq).unwrap_or(cursor);
                Ok(ChangefeedPage {
                    domain: domain.to_string(),
                    cursor,
                    next_cursor,
                    has_more: rows.len() == limit as usize,
                    rows,
                })
            })
            .map_err(Into::into)
    }

    /// Enumerate the module's currently live memory identities for a mirror upgrade.
    /// This keyset snapshot is applied before old feed tombstones may delete context rows.
    pub fn pull_live_memory_snapshot(
        &self,
        cursor: i64,
        limit: usize,
    ) -> Result<ChangefeedPage, McStoreError> {
        let limit = i64::try_from(limit.clamp(1, 1000))
            .map_err(|_| McStoreError::Serde("snapshot limit exceeds i64".to_string()))?;
        self.inner
            .with_conn(|conn| {
                // The resnapshot consumer heals mirror rows from these snapshots, so they
                // must carry the complete row like every other memories feed emission —
                // a reduced projection here would null-clobber healed columns downstream.
                let mut stmt = conn.prepare(&format!(
                    "{MEMORY_FULL_SELECT_COLUMNS} FROM mc_memories WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
                ))?;
                let rows = stmt
                    .query_map(params![cursor, limit], |row| {
                        let memory = stored_memory_full_from_row(row)?;
                        Ok(ChangefeedRow {
                            feed_seq: 0,
                            domain: "memories".to_string(),
                            op: "insert".to_string(),
                            module_row_id: memory.id,
                            content_hash: Some(memory.normalized_hash.clone()),
                            full_row_snapshot: memory_feed_snapshot(&memory, None),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let next_cursor = rows.last().map(|row| row.module_row_id).unwrap_or(cursor);
                Ok(ChangefeedPage {
                    domain: "memories".to_string(),
                    cursor,
                    next_cursor,
                    has_more: rows.len() == limit as usize,
                    rows,
                })
            })
            .map_err(Into::into)
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

fn replace_workspace_tx(
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

fn authority_route_context_store_uuid_tx(
    tx: &rusqlite::Transaction<'_>,
    route_project_root: &str,
) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT context_store_uuid
           FROM mc_authority_route_bindings
          WHERE route_project_root = ?1",
        params![route_project_root],
        |row| row.get(0),
    )
    .optional()
}

fn authority_memory_id_for_source_tx(
    tx: &rusqlite::Transaction<'_>,
    context_store_uuid: Option<&str>,
    context_row_id: i64,
) -> rusqlite::Result<Option<i64>> {
    let Some(context_store_uuid) = context_store_uuid else {
        return Ok(None);
    };
    tx.query_row(
        "SELECT id FROM mc_memories
          WHERE context_store_uuid = ?1 AND context_row_id = ?2",
        params![context_store_uuid, context_row_id],
        |row| row.get(0),
    )
    .optional()
}

fn replace_authority_memories_tx(
    tx: &rusqlite::Transaction<'_>,
    route_project_root: &str,
    memories: &[ShadowMemoryRow],
) -> rusqlite::Result<()> {
    let context_store_uuid = authority_route_context_store_uuid_tx(tx, route_project_root)?;
    for memory in memories {
        // The source row id is the context identity, not necessarily the module row id
        // allocated during the authority seed. Adopt that seeded row before the ordinary
        // id/content upserts so a mirror-back update cannot create a second module row.
        let superseded_by_memory_id = memory
            .superseded_by_memory_id
            .map(|source_id| {
                authority_memory_id_for_source_tx(tx, context_store_uuid.as_deref(), source_id)
                    .map(|translated| translated.unwrap_or(source_id))
            })
            .transpose()?;
        if let Some(existing_id) =
            authority_memory_id_for_source_tx(tx, context_store_uuid.as_deref(), memory.id)?
        {
            tx.execute(
                "UPDATE mc_memories
                    SET project_path = ?2, category = ?3, content = ?4, normalized_hash = ?5,
                        importance = ?6, scope = ?7, shareable = ?8, source_session_id = ?9,
                        source_type = ?10, seen_count = ?11, retrieval_count = ?12,
                        first_seen_at = ?13, created_at = ?14, updated_at = ?15, last_seen_at = ?16,
                        last_retrieved_at = ?17, status = ?18, expires_at = ?19,
                        verification_status = ?20, verified_at = ?21, classified_at = ?22,
                        superseded_by_memory_id = ?23, merged_from = ?24, metadata_json = ?25
                  WHERE id = ?1",
                params![
                    existing_id,
                    &memory.project_path,
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
                    superseded_by_memory_id,
                    memory.merged_from.as_deref(),
                    memory.metadata_json.as_deref(),
                ],
            )?;
            continue;
        }
        tx.execute(
            "INSERT INTO mc_memories
               (id, project_path, category, content, normalized_hash, importance,
                scope, shareable, source_session_id, source_type, seen_count, retrieval_count,
                first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
                status, expires_at, verification_status, verified_at, classified_at,
                superseded_by_memory_id, merged_from, metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)
             ON CONFLICT(id) DO UPDATE SET
                project_path = excluded.project_path,
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
                metadata_json = excluded.metadata_json
              ON CONFLICT(project_path, category, normalized_hash) DO UPDATE SET
                 content = excluded.content,
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
                memory.id,
                &memory.project_path,
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
                 superseded_by_memory_id,
                 memory.merged_from.as_deref(),
                memory.metadata_json.as_deref(),
            ],
        )?;
    }
    Ok(())
}

fn replace_shadow_user_profile_tx(
    tx: &rusqlite::Transaction<'_>,
    shadow_project_path: &str,
    profile_lines: &[String],
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM shadow_user_profile WHERE shadow_project_path = ?1",
        params![shadow_project_path],
    )?;
    for (profile_index, content) in profile_lines.iter().enumerate() {
        tx.execute(
            "INSERT INTO shadow_user_profile (shadow_project_path, profile_index, content)
             VALUES (?1, ?2, ?3)",
            params![shadow_project_path, profile_index as i64, content],
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

fn replace_authority_memory_mutations_tx(
    tx: &rusqlite::Transaction<'_>,
    route_project_root: &str,
    mutations: &[ShadowMemoryMutationRow],
) -> rusqlite::Result<()> {
    let context_store_uuid = authority_route_context_store_uuid_tx(tx, route_project_root)?;
    for row in mutations {
        let mutation = &row.mutation;
        let target_memory_id = authority_memory_id_for_source_tx(
            tx,
            context_store_uuid.as_deref(),
            mutation.target_memory_id,
        )?
        .unwrap_or(mutation.target_memory_id);
        let superseded_by_id = mutation
            .superseded_by_id
            .map(|source_id| {
                authority_memory_id_for_source_tx(tx, context_store_uuid.as_deref(), source_id)
                    .map(|translated| translated.unwrap_or(source_id))
            })
            .transpose()?;
        tx.execute(
            "INSERT INTO mc_memory_mutation_log
               (id, project_path, mutation_type, target_memory_id, superseded_by_id,
                category, new_content, queued_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET
                project_path = excluded.project_path,
                mutation_type = excluded.mutation_type,
                target_memory_id = excluded.target_memory_id,
                superseded_by_id = excluded.superseded_by_id,
                category = excluded.category,
                new_content = excluded.new_content,
                queued_at = excluded.queued_at",
            params![
                mutation.id,
                &row.project_path,
                &mutation.mutation_type,
                target_memory_id,
                superseded_by_id,
                mutation.category.as_deref(),
                mutation.new_content.as_deref(),
                mutation.queued_at,
            ],
        )?;
    }
    Ok(())
}

fn replace_authority_user_profile_tx(
    tx: &rusqlite::Transaction<'_>,
    profile_lines: &[String],
) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM mc_user_memories", [])?;
    for (index, content) in profile_lines.iter().enumerate() {
        tx.execute(
            "INSERT INTO mc_user_memories
                (content, status, promoted_at, source_candidate_ids, created_at, updated_at)
             VALUES (?1, 'active', ?2, '[]', 0, 0)",
            params![content, index as i64],
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

fn insert_historian_events_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    events: &[HistorianEventCandidate],
) -> rusqlite::Result<()> {
    for event in events {
        tx.execute(
            "INSERT INTO mc_compartment_events
               (session_id, compartment_id, at_compartment, kind, fields_json, created_at, harness)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'module')",
            params![
                session_id,
                event.compartment_id.map(|v| v as i64),
                event.at_compartment.map(|v| v as i64),
                event.kind,
                event.fields_json,
                event.created_at,
            ],
        )?;
    }
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

const NOTE_SELECT_COLUMNS: &str = "id, type, project_path, session_id, content, status, surface_condition, ready_at, ready_reason, manifest_json, compiled_check, check_hash, check_cron, check_failure_count, check_network_failure_count, check_quarantined_until, check_next_due_at, check_compiled_at, check_false_since_at, check_last_liveness_at, last_checked_at, check_status, check_version, policy_version, harness, anchor_block_id, anchor_ordinal, dismissed_at, dismissal_resolution, status_version, created_at_ms, updated_at_ms, context_store_uuid, context_row_id";
const NOTE_INSERT_COLUMNS: &str = "type, project_path, session_id, content, status, surface_condition, ready_at, ready_reason, manifest_json, compiled_check, check_hash, check_cron, check_failure_count, check_network_failure_count, check_quarantined_until, check_next_due_at, check_compiled_at, check_false_since_at, check_last_liveness_at, last_checked_at, check_status, check_version, policy_version, harness, anchor_block_id, anchor_ordinal, dismissed_at, dismissal_resolution, status_version, created_at_ms, updated_at_ms, context_store_uuid, context_row_id";

fn stored_note_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNote> {
    Ok(StoredNote {
        id: r.get(0)?,
        type_name: r.get(1)?,
        project_path: r.get(2)?,
        session_id: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        content: r.get(4)?,
        status: r.get(5)?,
        surface_condition: r.get(6)?,
        ready_at: r.get(7)?,
        ready_reason: r.get(8)?,
        manifest_json: r.get(9)?,
        compiled_check: r.get(10)?,
        check_hash: r.get(11)?,
        check_cron: r.get(12)?,
        check_failure_count: r.get(13)?,
        check_network_failure_count: r.get(14)?,
        check_quarantined_until: r.get(15)?,
        check_next_due_at: r.get(16)?,
        check_compiled_at: r.get(17)?,
        check_false_since_at: r.get(18)?,
        check_last_liveness_at: r.get(19)?,
        last_checked_at: r.get(20)?,
        check_status: r.get(21)?,
        check_version: r.get(22)?,
        policy_version: r.get(23)?,
        harness: r.get(24)?,
        anchor_block_id: r.get(25)?,
        anchor_ordinal: r.get(26)?,
        dismissed_at: r.get(27)?,
        dismissal_resolution: r.get(28)?,
        status_version: r.get(29)?,
        created_at_ms: r.get(30)?,
        updated_at_ms: r.get(31)?,
        context_store_uuid: r.get(32)?,
        context_row_id: r.get(33)?,
    })
}

fn load_note_tx(tx: &rusqlite::Transaction<'_>, id: i64) -> rusqlite::Result<StoredNote> {
    tx.query_row(
        &format!("SELECT {NOTE_SELECT_COLUMNS} FROM mc_notes WHERE id = ?1"),
        params![id],
        stored_note_from_row,
    )
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

const MEMORY_FULL_SELECT_COLUMNS: &str =
    "SELECT id, project_path, category, content, normalized_hash, importance, scope,
            shareable, source_session_id, source_type, seen_count, retrieval_count,
            first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
            status, expires_at, verification_status, verified_at, classified_at,
            superseded_by_memory_id, merged_from, metadata_json,
            context_store_uuid, context_row_id";

const MEMORY_FULL_SELECT_BY_ID: &str =
    "SELECT id, project_path, category, content, normalized_hash, importance, scope,
            shareable, source_session_id, source_type, seen_count, retrieval_count,
            first_seen_at, created_at, updated_at, last_seen_at, last_retrieved_at,
            status, expires_at, verification_status, verified_at, classified_at,
            superseded_by_memory_id, merged_from, metadata_json,
            context_store_uuid, context_row_id
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
        context_store_uuid: r.get(25)?,
        context_row_id: r.get(26)?,
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

fn memory_feed_snapshot(memory: &StoredMemoryFull, mapping: Option<&[String]>) -> Value {
    serde_json::json!({
        "id": memory.id,
        "project_path": memory.project_path,
        "category": memory.category,
        "content": memory.content,
        "normalized_hash": memory.normalized_hash,
        "importance": memory.importance,
        "scope": memory.scope,
        "shareable": memory.shareable,
        "source_session_id": memory.source_session_id,
        "source_type": memory.source_type,
        "seen_count": memory.seen_count,
        "retrieval_count": memory.retrieval_count,
        "first_seen_at": memory.first_seen_at,
        "created_at": memory.created_at,
        "updated_at": memory.updated_at,
        "last_seen_at": memory.last_seen_at,
        "last_retrieved_at": memory.last_retrieved_at,
        "status": memory.status,
        "expires_at": memory.expires_at,
        "verification_status": memory.verification_status,
        "verified_at": memory.verified_at,
        "classified_at": memory.classified_at,
        "superseded_by_memory_id": memory.superseded_by_memory_id,
        "merged_from": memory.merged_from,
        "metadata_json": memory.metadata_json,
        "context_store_uuid": memory.context_store_uuid,
        "context_row_id": memory.context_row_id,
        "mapping": mapping,
    })
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
    // Own rows keep the get-action's broad visibility. Foreign rows must satisfy the
    // complete canonical predicate even when the caller requests a specific id.
    let foreign_policy = if FOREIGN_VISIBLE_SQL.contains("shareable = 1") {
        " AND status IN ('active','permanent') AND (expires_at IS NULL OR expires_at > CAST(strftime('%s', 'now') AS INTEGER) * 1000) AND shareable = 1 AND scope IN ('project','ecosystem','universe')"
    } else {
        ""
    };
    let mut sharing = format!("{path_column} = ?");
    if !foreign.is_empty() && !share_categories.is_empty() {
        let fph = std::iter::repeat_n("?", foreign.len())
            .collect::<Vec<_>>()
            .join(", ");
        let cph = std::iter::repeat_n("?", share_categories.len())
            .collect::<Vec<_>>()
            .join(", ");
        sharing.push_str(&format!(
            " OR ({path_column} IN ({fph}) AND category IN ({cph}){foreign_policy})"
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
fn canonical_authority_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_authority_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_authority_value(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

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

fn wrapup_replaced_failure_summary(summary: &str, failed_created_at: i64) -> String {
    const SUMMARY_MAX_CHARS: usize = 500;

    let suffix = format!("; replaced failed record from {failed_created_at}");
    let prefix_chars = SUMMARY_MAX_CHARS.saturating_sub(suffix.chars().count());
    format!(
        "{}{}",
        summary.chars().take(prefix_chars).collect::<String>(),
        suffix
    )
}

fn capped_trace_error(error: &str) -> String {
    error.chars().take(2000).collect()
}

#[cfg(test)]
fn assert_memory_feed_snapshots_complete(store: &McStore) {
    let (columns, snapshots) = store
        .inner
        .with_conn(|conn| {
            let mut columns_statement = conn.prepare("PRAGMA table_info(mc_memories)")?;
            let columns = columns_statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut snapshots_statement = conn.prepare(
                "SELECT full_row_snapshot FROM mc_changefeed WHERE domain = 'memories' ORDER BY feed_seq",
            )?;
            let snapshots = snapshots_statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((columns, snapshots))
        })
        .unwrap();
    assert_eq!(
        columns,
        MEMORY_FEED_COLUMNS
            .iter()
            .map(|column| (*column).to_string())
            .collect::<Vec<_>>(),
        "the shared feed-column list must track the mc_memories schema"
    );
    // This invariant intentionally checks every operation's emitted row rather than listing
    // operation-specific keys, so a new mc_memories column cannot silently disappear from a feed.
    for serialized in snapshots {
        let snapshot: Value = serde_json::from_str(&serialized).unwrap();
        let object = snapshot.as_object().unwrap();
        for column in MEMORY_FEED_COLUMNS {
            assert!(
                object.contains_key(*column),
                "memory feed snapshot omitted column {column}: {serialized}"
            );
        }
    }
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
            route_project_root: None,
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
    fn transform_session_root_lineage_is_cache_committed_and_pruned_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        let commit_root = |session_id: &str, expected: Option<u64>, observed_at: i64| {
            let loaded = store.load(session_id).unwrap();
            store
                .commit_transform(
                    session_id,
                    TransformCommit {
                        expected,
                        core: &loaded.core,
                        meta: &loaded.meta,
                        consumed_drop_ids: &[],
                        first_applied_command_ids: &[],
                        memory_revision: None,
                        compartment_max_seq: None,
                        project_root: Some("/root-a"),
                        overlays: TransformOverlayBatch {
                            created_at_ms: observed_at,
                            ..Default::default()
                        },
                    },
                )
                .unwrap()
        };

        commit_root("refreshed", None, 1);
        commit_root("refreshed", Some(1), now_ms);
        commit_root("idle-live", None, 1);
        commit_root("deleted", None, 1);
        store
            .inner
            .with_conn(|conn| {
                conn.execute(
                    "DELETE FROM mc_cache_state WHERE session_id = 'deleted'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        drop(store);

        let reopened = McStore::open(&descriptor(dir.path())).unwrap();
        assert!(reopened
            .knows_transform_session_root("refreshed", "/root-a")
            .unwrap());
        assert!(!reopened
            .knows_transform_session_root("refreshed", "/root-b")
            .unwrap());
        assert!(reopened
            .knows_transform_session_root("idle-live", "/root-a")
            .unwrap());
        assert!(reopened.has_cache_state("idle-live").unwrap());
        assert!(!reopened
            .knows_transform_session_root("deleted", "/root-a")
            .unwrap());
        assert!(!reopened.has_cache_state("deleted").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn transform_session_roots_canonicalize_writes_and_match_legacy_symlink_rows() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let canonical_target = canonical_root(&target);
        let target_text = canonical_target.to_str().unwrap();
        let link_text = link.to_str().unwrap();

        let initial = store.load("canonical-write").unwrap();
        store
            .commit_transform(
                "canonical-write",
                TransformCommit {
                    expected: initial.row_version,
                    core: &initial.core,
                    meta: &initial.meta,
                    consumed_drop_ids: &[],
                    first_applied_command_ids: &[],
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: Some(link_text),
                    overlays: TransformOverlayBatch::default(),
                },
            )
            .unwrap();
        let stored_root: String = store
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT project_root FROM mc_transform_session_roots
                      WHERE session_id = 'canonical-write'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored_root, target_text);
        assert!(store
            .knows_transform_session_root("canonical-write", link_text)
            .unwrap());
        assert!(store
            .knows_transform_session_root("canonical-write", target_text)
            .unwrap());

        // Simulate a pre-migration row that retained the symlink spelling.
        store
            .commit(
                "legacy-row",
                None,
                &CoreState::default(),
                &ModuleMeta::default(),
            )
            .unwrap();
        store
            .inner
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO mc_transform_session_roots
                         (session_id, project_root, observed_at)
                     VALUES ('legacy-row', ?1, 1)",
                    params![link_text],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(store
            .knows_transform_session_root("legacy-row", target_text)
            .unwrap());
        assert!(store
            .knows_transform_session_root("legacy-row", link_text)
            .unwrap());

        let missing = dir.path().join("gone");
        assert_eq!(canonical_root(&missing), missing);
        let missing_text = missing.to_str().unwrap();
        let missing_initial = store.load("missing-root").unwrap();
        store
            .commit_transform(
                "missing-root",
                TransformCommit {
                    expected: missing_initial.row_version,
                    core: &missing_initial.core,
                    meta: &missing_initial.meta,
                    consumed_drop_ids: &[],
                    first_applied_command_ids: &[],
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: Some(missing_text),
                    overlays: TransformOverlayBatch::default(),
                },
            )
            .unwrap();
        assert!(store
            .knows_transform_session_root("missing-root", missing_text)
            .unwrap());
        assert!(!store
            .knows_transform_session_root("missing-root", "/another/gone")
            .unwrap());
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
    fn transform_snapshot_resists_commit_between_state_and_overlay_reads() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor(dir.path());
        let store = McStore::open(&descriptor).unwrap();
        let initial = store.load("ses").unwrap();
        store
            .commit("ses", initial.row_version, &initial.core, &initial.meta)
            .unwrap();
        let raw_path = match &descriptor.backend {
            StorageBackend::Sqlite { path } => path,
            _ => unreachable!("test descriptor is SQLite"),
        };
        let mut raw = rusqlite::Connection::open(raw_path).unwrap();
        raw.pragma_update(None, "busy_timeout", 5_000).unwrap();
        let core_json = serde_json::to_string(&initial.core).unwrap();
        let meta_json = serde_json::to_string(&initial.meta).unwrap();

        let snapshot = store
            .load_transform_snapshot_with_hook("ses", || {
                let transaction = raw.transaction().unwrap();
                transaction
                    .execute(
                        "UPDATE mc_cache_state SET row_version = 2, core_state = ?2, meta = ?3
                         WHERE session_id = ?1",
                        params!["ses", core_json, meta_json],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO mc_tags
                             (session_id, tag_number, block_id, kind, token_count, created_at_ms, source_bytes)
                         VALUES (?1, 1, 'm1#0', 'message', 1, 10, ?2)",
                        params!["ses", b"text".as_slice()],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO mc_overlay_frontiers (session_id, max_seen_ordinal)
                         VALUES (?1, 1)",
                        params!["ses"],
                    )
                    .unwrap();
                transaction.commit().unwrap();
            })
            .unwrap();
        assert_eq!(snapshot.loaded.row_version, Some(1));
        assert!(snapshot.tags.is_empty());
        assert_eq!(snapshot.overlay_frontier, None);
        let current = store.load_transform_snapshot("ses").unwrap();
        assert_eq!(current.loaded.row_version, Some(2));
        assert_eq!(current.tags.len(), 1);
        assert_eq!(current.overlay_frontier, Some(1));
    }

    #[test]
    fn transform_snapshot_keeps_row_version_and_overlays_from_one_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let initial = store.load("ses").unwrap();
        store
            .commit("ses", initial.row_version, &initial.core, &initial.meta)
            .unwrap();

        let split_state = store.load("ses").unwrap();
        let tag_mints = [TagMintInput {
            block_id: "m1#0".to_string(),
            kind: "message".to_string(),
            token_count: 4,
            source_bytes: b"authored text".to_vec(),
        }];
        let temporal_marks = [TemporalMarkInput {
            ordinal: 1,
            block_id: "m1#0".to_string(),
            marker_text: "<!-- +5m -->\n".to_string(),
        }];
        let hint = UserHintDecisionInput {
            ordinal: 1,
            block_id: "m1#0".to_string(),
            hint_text: "<ctx-search-hint>memory</ctx-search-hint>".to_string(),
        };
        let channel1 = Channel1AppendRow {
            block_id: "m1#0".to_string(),
            reminder_text: "<system-reminder>reduce</system-reminder>".to_string(),
            fired_at_ms: 10,
        };
        store
            .commit_transform(
                "ses",
                TransformCommit {
                    expected: split_state.row_version,
                    core: &split_state.core,
                    meta: &split_state.meta,
                    consumed_drop_ids: &[],
                    first_applied_command_ids: &[],
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: None,
                    overlays: TransformOverlayBatch {
                        max_seen_ordinal: Some(1),
                        tag_mints: &tag_mints,
                        temporal_marks: &temporal_marks,
                        user_hint: Some(&hint),
                        channel1_append: Some(&channel1),
                        created_at_ms: 10,
                    },
                },
            )
            .unwrap();

        let split_tags = store.load_tags_for_session("ses").unwrap();
        assert_eq!(split_state.row_version, Some(1));
        assert_eq!(
            split_tags.len(),
            1,
            "a split read can mix v1 state with v2 overlays"
        );

        let snapshot = store.load_transform_snapshot("ses").unwrap();
        assert_eq!(snapshot.loaded.row_version, Some(2));
        assert_eq!(snapshot.tags.len(), 1);
        assert_eq!(snapshot.temporal_marks.len(), 1);
        assert_eq!(snapshot.user_hints.len(), 1);
        assert_eq!(snapshot.channel1_appends.len(), 1);
        assert_eq!(snapshot.overlay_frontier, Some(1));
    }

    #[test]
    fn transform_cas_conflict_leaves_every_overlay_table_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let initial = store.load("ses").unwrap();
        store
            .commit("ses", initial.row_version, &initial.core, &initial.meta)
            .unwrap();
        let stale = store.load("ses").unwrap();
        store
            .commit("ses", stale.row_version, &stale.core, &stale.meta)
            .unwrap();

        let tags = [TagMintInput {
            block_id: "m1#0".to_string(),
            kind: "message".to_string(),
            token_count: 1,
            source_bytes: b"text".to_vec(),
        }];
        let marks = [TemporalMarkInput {
            ordinal: 1,
            block_id: "m1#0".to_string(),
            marker_text: "marker".to_string(),
        }];
        let hint = UserHintDecisionInput {
            ordinal: 1,
            block_id: "m1#0".to_string(),
            hint_text: "hint".to_string(),
        };
        let channel1 = Channel1AppendRow {
            block_id: "m1#0".to_string(),
            reminder_text: "reminder".to_string(),
            fired_at_ms: 1,
        };
        let error = store
            .commit_transform(
                "ses",
                TransformCommit {
                    expected: stale.row_version,
                    core: &stale.core,
                    meta: &stale.meta,
                    consumed_drop_ids: &[],
                    first_applied_command_ids: &[],
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: None,
                    overlays: TransformOverlayBatch {
                        max_seen_ordinal: Some(1),
                        tag_mints: &tags,
                        temporal_marks: &marks,
                        user_hint: Some(&hint),
                        channel1_append: Some(&channel1),
                        created_at_ms: 1,
                    },
                },
            )
            .unwrap_err();
        assert!(matches!(error, McStoreError::CasConflict { .. }));
        let snapshot = store.load_transform_snapshot("ses").unwrap();
        assert!(snapshot.tags.is_empty());
        assert!(snapshot.temporal_marks.is_empty());
        assert!(snapshot.user_hints.is_empty());
        assert!(snapshot.channel1_appends.is_empty());
        assert_eq!(snapshot.overlay_frontier, None);
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
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                1,
                false,
            )
            .unwrap();
        assert_eq!(
            first,
            AppendOutcome {
                queued: 1,
                duplicate: false,
                disposition: None,
            }
        );
        let pending = store.load_pending_agent_drops("ses").unwrap();

        let retry = store
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                2,
                false,
            )
            .unwrap();
        assert_eq!(
            retry,
            AppendOutcome {
                queued: 0,
                duplicate: true,
                disposition: None,
            }
        );
        assert_eq!(store.load_pending_agent_drops("ses").unwrap(), pending);
        assert_eq!(command_ledger_ids(&store, "ses"), vec!["tool-use-1"]);
    }

    #[test]
    fn first_application_marker_is_atomic_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor(dir.path());
        let store = McStore::open(&descriptor).unwrap();
        let targets = vec!["a#0".to_string(), "b#0".to_string()];
        store
            .append_pending_agent_drops_with_command("ses", Some("batch-1"), &targets, 1, false)
            .unwrap();
        let pending = store.load_pending_agent_drops("ses").unwrap();
        let loaded = store.load("ses").unwrap();
        let command_ids = vec!["batch-1".to_string()];
        store
            .commit_transform(
                "ses",
                TransformCommit {
                    expected: loaded.row_version,
                    core: &loaded.core,
                    meta: &loaded.meta,
                    consumed_drop_ids: &[pending[0].id],
                    first_applied_command_ids: &command_ids,
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: None,
                    overlays: TransformOverlayBatch::default(),
                },
            )
            .unwrap();
        let remaining = store.load_pending_agent_drops("ses").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].command_id.as_deref(), Some("batch-1"));
        assert_eq!(remaining[0].command_first_applied_at_ms, Some(0));
        drop(store);

        let reopened = McStore::open(&descriptor).unwrap();
        let persisted = reopened.load_pending_agent_drops("ses").unwrap();
        assert_eq!(persisted, remaining);
    }

    #[test]
    fn command_id_duplicate_survives_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];
        store
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                1,
                false,
            )
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
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                2,
                false,
            )
            .unwrap();
        assert_eq!(
            retry,
            AppendOutcome {
                queued: 0,
                duplicate: true,
                disposition: None,
            }
        );
    }

    #[test]
    fn different_command_id_requeues_after_consumption() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let target_ids = vec!["a#0".to_string()];
        store
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                1,
                false,
            )
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
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-2"),
                &target_ids,
                2,
                false,
            )
            .unwrap();
        assert_eq!(
            next,
            AppendOutcome {
                queued: 1,
                duplicate: false,
                disposition: None,
            }
        );
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
            .append_pending_agent_drops_with_command(
                "ses",
                Some("tool-use-1"),
                &target_ids,
                1,
                false
            )
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
                .append_pending_agent_drops_with_command(
                    "ses",
                    Some("tool-use-1"),
                    &target_ids,
                    2,
                    false
                )
                .unwrap(),
            AppendOutcome {
                queued: 1,
                duplicate: false,
                disposition: None,
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
                    false,
                )
                .unwrap();
            assert!(!outcome.duplicate);
        }

        let command_ids = command_ledger_ids(&store, "ses");
        assert_eq!(command_ids.len(), 513);
        assert_eq!(command_ids.first().map(String::as_str), Some("command-000"));
        assert_eq!(command_ids.last().map(String::as_str), Some("command-512"));

        let oldest_retry = store
            .append_pending_agent_drops_with_command(
                "ses",
                Some("command-000"),
                &target_ids,
                513,
                false,
            )
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
            .append_pending_agent_drops_with_command(
                session_id,
                Some("tool-use-1"),
                &target_ids,
                1,
                false,
            )
            .unwrap();
        store.reset_shadow_session(session_id, session_id).unwrap();

        assert_eq!(
            store
                .append_pending_agent_drops_with_command(
                    session_id,
                    Some("tool-use-1"),
                    &target_ids,
                    2,
                    false,
                )
                .unwrap(),
            AppendOutcome {
                queued: 1,
                duplicate: false,
                disposition: None,
            }
        );
    }

    #[test]
    fn zero_target_append_records_ledger_row_with_no_targets_disposition() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();

        // Zero targets: the ledger row is recorded with disposition='no_targets'
        // so a retry of the same command_id still dedupes.
        let outcome = store
            .append_pending_agent_drops_with_command("ses", Some("cmd-zero"), &[], 1, true)
            .unwrap();
        assert_eq!(
            outcome,
            AppendOutcome {
                queued: 0,
                duplicate: false,
                disposition: Some("no_targets".to_string()),
            }
        );
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        // Retry of the same command_id must still dedupe (idempotency).
        let retry = store
            .append_pending_agent_drops_with_command("ses", Some("cmd-zero"), &[], 2, true)
            .unwrap();
        assert_eq!(
            retry,
            AppendOutcome {
                queued: 0,
                duplicate: true,
                disposition: None,
            }
        );

        // A normal command with actual targets still queues pending drops and does not
        // set any disposition — the no_targets path only applies when canonicalization
        // resolved zero block_ids.
        let normal = store
            .append_pending_agent_drops_with_command(
                "ses",
                Some("cmd-normal"),
                &["a#0".to_string()],
                3,
                false,
            )
            .unwrap();
        assert_eq!(
            normal,
            AppendOutcome {
                queued: 1,
                duplicate: false,
                disposition: None,
            }
        );
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);

        // The no_targets row exists in the ledger (for idempotency) but has disposition set.
        let ledger_ids = command_ledger_ids(&store, "ses");
        assert!(ledger_ids.contains(&"cmd-zero".to_string()));
        assert!(ledger_ids.contains(&"cmd-normal".to_string()));
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
        assert_eq!(store.overlay_watermark("ses").unwrap(), None);
        store
            .apply_active_overlay_decisions("ses", 4, &[], None, 800)
            .unwrap();
        assert_eq!(store.overlay_watermark("ses").unwrap(), Some(4));
    }

    #[test]
    fn overlay_decisions_share_an_atomic_ordinal_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(McStore::open(&descriptor(dir.path())).unwrap());
        let initial = store.load("race").unwrap();
        store
            .commit("race", initial.row_version, &initial.core, &initial.meta)
            .unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut joins = Vec::new();
        for hint_text in ["winner-a", "winner-b"] {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                let loaded = store.load("race").unwrap();
                let temporal_marks = [TemporalMarkInput {
                    ordinal: 3,
                    block_id: "m3#0".to_string(),
                    marker_text: "<!-- +5m -->\n".to_string(),
                }];
                let hint = UserHintDecisionInput {
                    ordinal: 3,
                    block_id: "m3#0".to_string(),
                    hint_text: hint_text.to_string(),
                };
                barrier.wait();
                store.commit_transform(
                    "race",
                    TransformCommit {
                        expected: loaded.row_version,
                        core: &loaded.core,
                        meta: &loaded.meta,
                        consumed_drop_ids: &[],
                        first_applied_command_ids: &[],
                        memory_revision: None,
                        compartment_max_seq: None,
                        project_root: None,
                        overlays: TransformOverlayBatch {
                            max_seen_ordinal: Some(3),
                            temporal_marks: &temporal_marks,
                            user_hint: Some(&hint),
                            created_at_ms: 100,
                            ..Default::default()
                        },
                    },
                )
            }));
        }
        barrier.wait();
        let outcomes = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(McStoreError::CasConflict { .. })))
                .count(),
            1
        );
        let winner = store.load_user_hints("race").unwrap();
        assert_eq!(winner.len(), 1);
        assert!(matches!(
            winner[0].hint_text.as_str(),
            "winner-a" | "winner-b"
        ));
        assert_eq!(store.overlay_watermark("race").unwrap(), Some(3));
        assert_eq!(store.load_temporal_marks("race").unwrap().len(), 1);

        let loaded = store.load("race").unwrap();
        let late_mark = [TemporalMarkInput {
            ordinal: 2,
            block_id: "m2#0".to_string(),
            marker_text: "late marker".to_string(),
        }];
        let late_hint = UserHintDecisionInput {
            ordinal: 2,
            block_id: "m2#0".to_string(),
            hint_text: "late hint".to_string(),
        };
        store
            .commit_transform(
                "race",
                TransformCommit {
                    expected: loaded.row_version,
                    core: &loaded.core,
                    meta: &loaded.meta,
                    consumed_drop_ids: &[],
                    first_applied_command_ids: &[],
                    memory_revision: None,
                    compartment_max_seq: None,
                    project_root: None,
                    overlays: TransformOverlayBatch {
                        max_seen_ordinal: Some(4),
                        temporal_marks: &late_mark,
                        user_hint: Some(&late_hint),
                        created_at_ms: 200,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        assert_eq!(store.overlay_watermark("race").unwrap(), Some(4));
        assert_eq!(store.load_user_hints("race").unwrap().len(), 1);
        assert_eq!(store.load_temporal_marks("race").unwrap().len(), 1);
    }

    #[test]
    fn wrapup_command_ledger_keeps_the_first_terminal_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let first = store
            .record_wrapup_command("session", "command", "completed", 2, "done", 10)
            .unwrap();
        let retry = store
            .record_wrapup_command("session", "command", "nothing_to_compact", 9, "changed", 20)
            .unwrap();
        assert_eq!(retry, first);
        let rejected = store
            .record_wrapup_command("session", "failed-command", "failed", 9, "changed", 20)
            .unwrap_err();
        assert!(rejected
            .to_string()
            .contains("nonterminal wrapup disposition"));
        assert_eq!(
            store.load_wrapup_command("session", "command").unwrap(),
            Some(first)
        );
        assert!(store
            .load_wrapup_command("session", "failed-command")
            .unwrap()
            .is_none());
        assert!(store
            .load_wrapup_command("session", "other")
            .unwrap()
            .is_none());
    }

    #[test]
    fn todo_state_set_and_soft_refresh_are_replay_safe() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let state = r#"[{"content":"one","status":"pending","priority":"medium"}]"#;
        let first = store
            .set_todo_state("session", state, "m1", "hash-1")
            .unwrap();
        assert!(matches!(
            first,
            TodoStateSetOutcome::Updated { row_version: 1 }
        ));
        assert!(matches!(
            store.set_todo_state("session", state, "m1", "hash-1"),
            Ok(TodoStateSetOutcome::Noop)
        ));
        let changed_owner = store
            .set_todo_state("session", state, "m2", "hash-1")
            .unwrap();
        assert!(matches!(
            changed_owner,
            TodoStateSetOutcome::Updated { row_version: 2 }
        ));
        assert!(matches!(
            store.set_todo_state("session", "[]", "m2", "hash-2"),
            Ok(TodoStateSetOutcome::Updated { row_version: 3 })
        ));

        assert!(store.arm_soft_refresh("session").unwrap());
        let armed = store.load("session").unwrap();
        assert!(armed.meta.soft_refresh_pending);
        assert!(store.arm_soft_refresh("session").unwrap());
        let still_armed = store.load("session").unwrap();
        assert_eq!(still_armed.row_version, armed.row_version);

        let first_recomp = store
            .record_recomp_command("session", "recomp-1", "nothing_to_do", 10)
            .unwrap();
        let replay = store
            .record_recomp_command("session", "recomp-1", "started", 20)
            .unwrap();
        assert_eq!(replay, first_recomp);
        assert_eq!(
            store.load_recomp_command("session", "recomp-1").unwrap(),
            Some(first_recomp)
        );
    }

    #[test]
    fn wrapup_command_recording_is_fenced_by_row_version_and_revert_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let initial = store.load("session").unwrap();
        let row_version = store
            .commit("session", initial.row_version, &initial.core, &initial.meta)
            .unwrap();
        let stale = store
            .record_wrapup_command_if_current(WrapupCommandRecord {
                session_id: "session",
                command_id: "stale",
                disposition: "completed",
                rounds: 1,
                summary: "done",
                created_at: 10,
                expected_row_version: Some(row_version - 1),
                expected_revert_epoch: 0,
            })
            .unwrap();
        assert!(matches!(stale, RecordWrapupCommandOutcome::Stale { .. }));
        assert!(store
            .load_wrapup_command("session", "stale")
            .unwrap()
            .is_none());

        let recorded = store
            .record_wrapup_command_if_current(WrapupCommandRecord {
                session_id: "session",
                command_id: "current",
                disposition: "completed",
                rounds: 1,
                summary: "done",
                created_at: 10,
                expected_row_version: Some(row_version),
                expected_revert_epoch: 0,
            })
            .unwrap();
        assert!(matches!(
            recorded,
            RecordWrapupCommandOutcome::Recorded(WrapupCommandRow { ref disposition, .. })
                if disposition == "completed"
        ));
    }

    #[test]
    fn wrapup_command_recording_replaces_legacy_failure_with_capped_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let initial = store.load("session").unwrap();
        let row_version = store
            .commit("session", initial.row_version, &initial.core, &initial.meta)
            .unwrap();
        store
            .seed_legacy_wrapup_command_for_test(
                "session",
                "legacy",
                "failed",
                1,
                "old failure",
                17,
            )
            .unwrap();
        let summary = "é".repeat(600);

        let outcome = store
            .record_wrapup_command_if_current(WrapupCommandRecord {
                session_id: "session",
                command_id: "legacy",
                disposition: "completed",
                rounds: 3,
                summary: &summary,
                created_at: 99,
                expected_row_version: Some(row_version),
                expected_revert_epoch: 0,
            })
            .unwrap();
        let RecordWrapupCommandOutcome::Recorded(recorded) = outcome else {
            panic!("legacy failure must be atomically replaced");
        };
        assert_eq!(recorded.disposition, "completed");
        assert_eq!(recorded.rounds, 3);
        assert_eq!(recorded.created_at, 99);
        assert!(recorded.summary.chars().count() <= 500);
        assert!(recorded
            .summary
            .ends_with("; replaced failed record from 17"));
        assert_eq!(
            store.load_wrapup_command("session", "legacy").unwrap(),
            Some(recorded)
        );
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
        let expected_versions = (1_i64..=37).collect::<Vec<_>>();
        let fresh_versions = fresh
            .inner
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT version FROM cortexkit_schema_version
                     WHERE namespace = ?1 ORDER BY version",
                )?;
                let versions = statement
                    .query_map(params![NS], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(versions)
            })
            .unwrap();
        assert_eq!(fresh_versions, expected_versions);
        assert!(fresh_versions.contains(&30));
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
        let fresh_migration_19_tables = fresh
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table'
                       AND name IN ('mc_overlay_frontiers', 'mc_temporal_marks', 'mc_wrapup_commands')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(fresh_migration_19_tables, 3);

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
                  ('shadow:test', 0, 'old-noise', 'old-ts', 'old-rs', '[]', '{}', '{}', 'old', 1),
                  ('shadow:test', 1, 'byte-mismatch', 'ts', 'rs', '[]', '{}', '{}', 'a', 1784222040001),
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
        let migrated_migration_19_tables = migrated
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table'
                       AND name IN ('mc_overlay_frontiers', 'mc_temporal_marks', 'mc_wrapup_commands')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .unwrap();
        assert_eq!(migrated_migration_19_tables, 3);
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
            .append_pending_agent_drops_with_command("ledger", Some("command"), &[], 1, true)
            .unwrap();
        store.append_user_hint("hints", "m1#0", "", 1).unwrap();
        store
            .apply_active_overlay_decisions(
                "overlays",
                1,
                &[TemporalMarkInput {
                    ordinal: 1,
                    block_id: "m1#0".to_string(),
                    marker_text: String::new(),
                }],
                None,
                1,
            )
            .unwrap();
        store
            .record_wrapup_command("wrapup", "command", "completed", 1, "done", 1)
            .unwrap();

        for session_id in [
            "cache",
            "compartments",
            "tags",
            "pending",
            "ledger",
            "hints",
            "overlays",
            "wrapup",
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
                        scope, shareable, status, expires_at, first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (?1,?2,'ARCHITECTURE',?3,?4,?5,'project',1,?6,?7,0,0,0,0)",
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
    fn foreign_shareable_revocation_stays_on_m1_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:epoch-own";
        let foreign = "git:epoch-foreign";
        insert_memory(&store, foreign, 1, "shared", Some(50), "active", None);
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'epoch-ws','[\"CONSTRAINTS\",\"ARCHITECTURE\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at) VALUES (1, ?1, ?1, ?1, 0), (1, ?2, ?2, ?2, 0)",
                    params![own, foreign],
                )?;
                Ok(())
            })
            .unwrap();
        let before = store.workspace_fingerprint(own, 0).unwrap();
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute("UPDATE mc_memories SET shareable = 0 WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap();
        let after = store.workspace_fingerprint(own, 0).unwrap();
        assert_eq!(before, after);
        assert!(store
            .inner
            .with_conn(|conn| conn
                .query_row(
                    "SELECT epoch FROM mc_memory_visibility_epoch WHERE project_path = ?1",
                    params![foreign],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map(|value| value.is_none()))
            .unwrap());
    }

    #[test]
    fn foreign_expiry_transition_does_not_bump_visibility_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:expiry-own";
        let foreign = "git:expiry-foreign";
        insert_memory(
            &store,
            foreign,
            1,
            "shared until expiry",
            Some(50),
            "active",
            Some(i64::MAX),
        );
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'expiry-ws','[\"ARCHITECTURE\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at) VALUES (1, ?1, ?1, ?1, 0), (1, ?2, ?2, ?2, 0)",
                    params![own, foreign],
                )?;
                tx.execute("UPDATE mc_memories SET expires_at = 0 WHERE id = 1", [])?;
                tx.execute("UPDATE mc_memories SET shareable = 0 WHERE id = 1", [])?;
                // Idempotent no-op after the row is already non-visible ignoring expiry.
                tx.execute("UPDATE mc_memories SET shareable = 0 WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap();
        let epoch = store
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT epoch FROM mc_memory_visibility_epoch WHERE project_path = ?1",
                    params![foreign],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(epoch, None);
    }

    #[test]
    fn get_by_id_applies_full_foreign_visibility_but_keeps_own_archived_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:get-own";
        let foreign = "git:get-foreign";
        insert_memory(&store, foreign, 1, "private", Some(50), "active", None);
        insert_memory(&store, foreign, 2, "archived", Some(50), "archived", None);
        insert_memory(&store, foreign, 3, "expired", Some(50), "active", Some(0));
        insert_memory(&store, own, 4, "own archived", Some(50), "archived", None);
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'get-ws','[\"ARCHITECTURE\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at) VALUES (1, ?1, ?1, ?1, 0), (1, ?2, ?2, ?2, 0)",
                    params![own, foreign],
                )?;
                tx.execute(
                    "UPDATE mc_memories SET scope = 'project', shareable = CASE WHEN id = 1 THEN 0 ELSE 1 END",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let visible = store
            .get_visible_memories_by_ids(own, &[1, 2, 3, 4])
            .unwrap();
        assert!(!visible.contains_key(&1), "foreign shareable=0 row leaked");
        assert!(!visible.contains_key(&2), "foreign archived row leaked");
        assert!(!visible.contains_key(&3), "foreign expired row leaked");
        assert_eq!(
            visible.get(&4).map(|row| row.status.as_str()),
            Some("archived")
        );
    }

    #[test]
    fn workspace_fingerprint_is_deterministic_and_membership_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        // a project in NO workspace → stable empty marker
        assert_eq!(store.workspace_fingerprint(own, 0).unwrap(), "");

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
        let fp1 = store.workspace_fingerprint(own, 0).unwrap();
        let fp2 = store.workspace_fingerprint(own, 0).unwrap();
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
        let fp3 = store.workspace_fingerprint(own, 0).unwrap();
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
    fn matching_historian_abandon_fences_predicate_and_update_for_both_backoffs() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor(dir.path());
        let raw_path = match &descriptor.backend {
            StorageBackend::Sqlite { path } => path.clone(),
            _ => unreachable!("test descriptor is SQLite"),
        };
        let store = McStore::open(&descriptor).unwrap();
        store
            .commit("ses", None, &CoreState::default(), &publishing_meta())
            .unwrap();

        let hook_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls_for_hook = std::sync::Arc::clone(&hook_calls);
        store.set_abandon_historian_hook(Box::new(move || {
            hook_calls_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let raw = rusqlite::Connection::open(&raw_path).unwrap();
            raw.busy_timeout(std::time::Duration::ZERO).unwrap();
            let error = raw
                .execute(
                    "UPDATE mc_cache_state SET row_version = row_version WHERE session_id = ?1",
                    params!["ses"],
                )
                .expect_err("BEGIN IMMEDIATE must block a competing writer");
            assert!(
                matches!(
                    error,
                    rusqlite::Error::SqliteFailure(ref error, _)
                        if error.code == rusqlite::ErrorCode::DatabaseBusy
                ),
                "the competing write must fail with SQLITE_BUSY: {error}"
            );
        }));

        let first_before = store.load("ses").unwrap();
        let first_abandoned = store
            .abandon_historian_run_if_matching(
                "ses",
                &publish_predicate(),
                None,
                Some("snapshot generation changed"),
            )
            .unwrap();
        assert_eq!(first_abandoned, Some(first_before.row_version.unwrap() + 1));
        let idle = store.load("ses").unwrap();
        assert_eq!(idle.row_version, first_abandoned);
        assert_eq!(idle.meta.historian.state, HistorianPhase::Idle);
        assert_eq!(idle.meta.historian.firing_seq, 7);
        assert_eq!(idle.meta.historian.failure_backoff_at_ms, None);
        assert_eq!(
            idle.meta.historian.last_failure.as_deref(),
            Some("snapshot generation changed")
        );

        let publishing = store.load("ses").unwrap();
        let publishing_meta = publishing_meta();
        store
            .commit(
                "ses",
                publishing.row_version,
                &publishing.core,
                &publishing_meta,
            )
            .unwrap();
        let second_before = store.load("ses").unwrap();
        let second_abandoned = store
            .abandon_historian_run_if_matching(
                "ses",
                &publish_predicate(),
                Some(999),
                Some("fingerprint or CAS conflict"),
            )
            .unwrap();
        assert_eq!(
            second_abandoned,
            Some(second_before.row_version.unwrap() + 1)
        );
        let cooled_down = store.load("ses").unwrap();
        assert_eq!(cooled_down.row_version, second_abandoned);
        assert_eq!(cooled_down.meta.historian.state, HistorianPhase::Idle);
        assert_eq!(cooled_down.meta.historian.failure_backoff_at_ms, Some(999));
        assert_eq!(
            cooled_down.meta.historian.last_failure.as_deref(),
            Some("fingerprint or CAS conflict")
        );
        assert_eq!(
            hook_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the fenced callback must run for both abandon variants"
        );
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                route_project_root: None,
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
                route_project_root: None,
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
        let before_dismiss = store
            .get_note_by_id("git:proj", "ses", first.id)
            .unwrap()
            .unwrap();
        let feed_before_dismiss = store.pull_changefeed("notes", 0, 100).unwrap().next_cursor;
        let dismissed = store
            .dismiss_note("git:proj", "ses", first.id, Some("done in v2"), 40)
            .unwrap()
            .unwrap();
        assert_eq!(dismissed.status, "dismissed");
        assert!(dismissed.content.contains("done in v2"));
        assert_eq!(dismissed.status_version, before_dismiss.status_version + 1);
        let dismissal_feed = store
            .pull_changefeed("notes", feed_before_dismiss, 100)
            .unwrap();
        assert_eq!(dismissal_feed.rows.len(), 1);
        assert_eq!(dismissal_feed.rows[0].module_row_id, first.id);
        assert_eq!(store.read_notes("git:proj", "ses", 25, 0).unwrap().len(), 1);
        assert!(store
            .search_notes_like("git:other", "ses", "pagination")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn project_notes_use_cas_and_at_least_once_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let note = store
            .insert_project_note(NoteWriteInput {
                project_path: "git:proj",
                route_project_root: None,
                session_id: Some("writer-session"),
                content: "wait for the release",
                surface_condition: Some("release exists"),
                anchor_block_id: None,
                anchor_ordinal: None,
                now_ms: 10,
            })
            .unwrap();
        assert_eq!(note.status, "pending");
        let note = match store
            .update_note_cas(
                "git:proj",
                note.id,
                "pending",
                note.status_version,
                Some("wait for the release tag"),
                None,
                11,
            )
            .unwrap()
        {
            NoteCasOutcome::Applied(note) => note,
            other => panic!("unexpected cold-start update outcome: {other:?}"),
        };
        assert!(store
            .read_project_notes("git:other", None, &["pending"], 25, 0)
            .unwrap()
            .is_empty());
        assert!(matches!(
            store.update_note_cas(
                "git:other",
                note.id,
                "pending",
                note.status_version,
                Some("cross-project write"),
                None,
                11,
            ),
            Err(McStoreError::NoteOwnershipMismatch { .. })
        ));

        let claimed = store.claim_due_note("git:proj", 20).unwrap().unwrap();
        let evaluated = store
            .write_note_evaluation(NoteEvaluationInput {
                project_path: "git:proj",
                note_id: note.id,
                source_revision: claimed.status_version,
                verdict: true,
                compiled_check: Some("release exists"),
                manifest_json: Some("{}"),
                check_hash: Some("hash"),
                next_due_at: None,
                now_ms: 30,
            })
            .unwrap();
        let ready = match evaluated {
            NoteCasOutcome::Applied(note) => note,
            other => panic!("unexpected evaluation outcome: {other:?}"),
        };
        assert_eq!(ready.status, "ready");

        let first = store
            .claim_note_delivery("git:proj", "serve-session", "pass-1", "pass-1", 40)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert!(!store
            .claim_note_delivery("git:proj", "serve-session", "pass-2", "pass-2", 50)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .ack_note_delivery("git:proj", "serve-session", "pass-2", 60)
                .unwrap(),
            1
        );
        // A newer acknowledged delivery closes the older lost attempt, so the
        // surfaced note cannot be delivered forever.
        assert!(store
            .claim_note_delivery("git:proj", "serve-session", "pass-3", "pass-3", 70)
            .unwrap()
            .is_empty());

        let surfaced = store
            .read_project_notes("git:proj", None, &["surfaced"], 25, 0)
            .unwrap()
            .into_iter()
            .find(|row| row.id == note.id)
            .unwrap();
        let dismissed = store
            .dismiss_note_cas(
                "git:proj",
                note.id,
                "surfaced",
                surfaced.status_version,
                Some("done"),
                80,
            )
            .unwrap();
        assert!(matches!(dismissed, NoteCasOutcome::Applied(note) if note.status == "dismissed"));
        assert!(store
            .claim_note_delivery("git:proj", "serve-session", "pass-4", "pass-4", 90)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn note_delivery_nack_is_terminal_and_late_ack_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let note = store
            .insert_project_note(NoteWriteInput {
                project_path: "git:proj",
                route_project_root: None,
                session_id: Some("writer"),
                content: "evaluate me",
                surface_condition: Some("condition"),
                anchor_block_id: None,
                anchor_ordinal: None,
                now_ms: 1,
            })
            .unwrap();
        store
            .write_note_evaluation(NoteEvaluationInput {
                project_path: "git:proj",
                note_id: note.id,
                source_revision: note.status_version,
                verdict: true,
                compiled_check: None,
                manifest_json: None,
                check_hash: None,
                next_due_at: None,
                now_ms: 2,
            })
            .unwrap();
        assert_eq!(
            store
                .claim_note_delivery("git:proj", "session", "fingerprint-1", "pass", 3)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .nack_note_delivery("git:proj", "session", "pass", 4)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .ack_note_delivery("git:proj", "session", "pass", 5)
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .claim_note_delivery("git:proj", "session", "fingerprint-2", "pass-2", 6)
                .unwrap()
                .len(),
            1,
            "a NACKed attempt is terminal, while the ready note may be retried in a new attempt"
        );
    }

    #[test]
    fn note_delivery_ack_is_scoped_by_project_session_and_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        for project in ["git:a", "git:b"] {
            let note = store
                .insert_project_note(NoteWriteInput {
                    project_path: project,
                    route_project_root: None,
                    session_id: Some("writer"),
                    content: "scoped note",
                    surface_condition: Some("condition"),
                    anchor_block_id: None,
                    anchor_ordinal: None,
                    now_ms: 1,
                })
                .unwrap();
            store
                .write_note_evaluation(NoteEvaluationInput {
                    project_path: project,
                    note_id: note.id,
                    source_revision: note.status_version,
                    verdict: true,
                    compiled_check: None,
                    manifest_json: None,
                    check_hash: None,
                    next_due_at: None,
                    now_ms: 2,
                })
                .unwrap();
            store
                .claim_note_delivery(project, "session", project, "shared-pass", 3)
                .unwrap();
        }
        assert_eq!(
            store
                .ack_note_delivery("git:a", "session", "shared-pass", 4)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .read_project_notes("git:a", None, &["surfaced"], 10, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .read_project_notes("git:b", None, &["surfacing"], 10, 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn note_lookup_and_visibility_paging_are_not_bounded_by_first_page() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let mut first_id = 0;
        for index in 0..105 {
            let note = store
                .insert_project_note(NoteWriteInput {
                    project_path: "git:proj",
                    route_project_root: None,
                    session_id: Some("writer"),
                    content: &format!("note {index}"),
                    surface_condition: None,
                    anchor_block_id: None,
                    anchor_ordinal: None,
                    now_ms: 1,
                })
                .unwrap();
            if index == 0 {
                first_id = note.id;
            }
        }
        let found = store
            .get_note_by_id("git:proj", "reader", first_id)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, first_id);
        let updated = store
            .update_note_content(
                "git:proj",
                "reader",
                first_id,
                "updated outside the first page",
                2,
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.content, "updated outside the first page");
        let page = store
            .read_visible_notes("git:proj", "reader", &["active"], 5, 100)
            .unwrap();
        assert_eq!(page.len(), 5);
        assert!(page.iter().all(|note| note.project_path == "git:proj"));
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
                promote_facts: true,
                events: &[],
                primer_candidates: &[],
                user_memory_candidates: &[],
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
        shared.shareable = 1;
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[own, shared, private],
                memory_mutations: &[],
                user_profile: &[],
                workspace: Some(&workspace),
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &compartments,
                memories: &memories,
                memory_mutations: &mutations,
                user_profile: &[],
                workspace: None,
                last_todo_state: Some("[]".to_string()),
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &initial,
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &replacement,
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &replacement,
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &compartments,
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
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
    fn drop_seed_units_are_frozen_before_transform_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "shadow:drop-seed";
        let seeds = vec![
            ShadowDropSeedRow {
                block_id: "m1#0".to_string(),
                related_block_ids: vec!["m1#1".to_string()],
                drop_mode: "truncated".to_string(),
                payload: None,
            },
            ShadowDropSeedRow {
                block_id: "m2#0".to_string(),
                related_block_ids: vec![],
                drop_mode: "edit_marker".to_string(),
                payload: Some("{\"filePath\":\"src/main.ts\"}".to_string()),
            },
        ];
        let first = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &seeds,
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(first.drop_seeds_skipped, 0);
        let first_state = store.load(session).unwrap();
        assert_eq!(first_state.core.frozen_units.len(), 3);
        assert_eq!(first_state.core.frozen_units[0].key, "red:m1#0");
        assert_eq!(first_state.core.frozen_units[0].kind, "skeleton");
        assert_eq!(first_state.core.frozen_units[0].frozen_payload, "[dropped]");
        assert_eq!(first_state.core.frozen_units[1].key, "red:m1#1");

        let second = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: session,
                shadow_generation: 0,
                expected_shadow_seq: 1,
                seed_boundary_id: None,
                drop_seeds: &seeds,
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(second.drop_seeds_skipped, 0);
        let second_state = store.load(session).unwrap();
        assert_eq!(
            second_state.core.frozen_units,
            first_state.core.frozen_units
        );
    }

    #[test]
    fn invalid_drop_seed_is_counted_without_rejecting_state_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let seeds = [ShadowDropSeedRow {
            block_id: "not-a-block".to_string(),
            related_block_ids: vec![],
            drop_mode: "full".to_string(),
            payload: None,
        }];
        let result = store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: "shadow:drop-seed-invalid",
                shadow_project_path: "shadow:drop-seed-invalid",
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &seeds,
                drop_seed_skipped: 3,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();
        assert_eq!(result.drop_seeds_skipped, 4);
        assert!(store
            .load("shadow:drop-seed-invalid")
            .unwrap()
            .core
            .frozen_units
            .is_empty());
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

    #[test]
    fn authority_route_binding_migration_merges_duplicates_and_rekeys_singletons() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let route_project_root = "/worktrees/repo";
        let identity = "git:identity";
        let insert = |project_path: &str, content: &str, now_ms| {
            store
                .insert_memory(InsertMemoryInput {
                    project_path,
                    route_project_root: None,
                    category: "CONSTRAINTS",
                    content,
                    source_session_id: None,
                    source_type: Some("agent"),
                    importance: Some(50),
                    expires_at: None,
                    metadata_json: None,
                    now_ms,
                })
                .unwrap()
        };
        let canonical = insert(identity, "same fact", 1);
        let duplicate = insert(route_project_root, "same fact", 2);
        let singleton = insert(route_project_root, "path-only fact", 3);
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES ('context', ?1, 'memories', 'MODULE')",
                    params![identity],
                )?;
                Ok(())
            })
            .unwrap();

        store
            .bind_authority_route("context", identity, route_project_root)
            .unwrap();

        assert!(store.get_memory_full(duplicate).unwrap().is_none());
        assert_eq!(
            store
                .get_memory_full(singleton)
                .unwrap()
                .unwrap()
                .project_path,
            identity
        );
        assert_eq!(
            store
                .get_memory_full(canonical)
                .unwrap()
                .unwrap()
                .project_path,
            identity
        );
        assert!(store
            .load_active_memories(route_project_root, 10)
            .unwrap()
            .is_empty());
        let feed = store.pull_changefeed("memories", 0, 100).unwrap();
        assert!(
            feed.rows
                .iter()
                .any(|row| row.module_row_id == duplicate && row.op == "tombstone"),
            "historical changefeed rows remain while the duplicate receives a tombstone"
        );
    }

    #[test]
    fn authority_route_binding_upgrade_normalizes_preexisting_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.create_scalar_function(
            "mc_note_caller_project",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_context| Ok(String::new()),
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE cortexkit_schema_version (
                 namespace TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 applied_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (namespace, version)
             );",
        )
        .unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 29)
        {
            conn.execute_batch(migration.statements).unwrap();
            conn.execute(
                "INSERT INTO cortexkit_schema_version(namespace, version, applied_at_unix)
                 VALUES (?1, ?2, 0)",
                params![NS, migration.version],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
             VALUES ('/worktrees/repo', 'context', 'git:identity')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_memories
                 (id, project_path, category, content, normalized_hash, importance, status,
                  first_seen_at, created_at, updated_at, last_seen_at)
             VALUES (1, 'git:identity', 'CONSTRAINTS', 'same', 'same-hash', 50, 'active', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_memories
                 (id, project_path, category, content, normalized_hash, importance, status,
                  first_seen_at, created_at, updated_at, last_seen_at)
             VALUES (2, '/worktrees/repo', 'CONSTRAINTS', 'same', 'same-hash', 50, 'active', 0, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
             VALUES ('context', 'git:identity', 'memories', 'MODULE')",
            [],
        )
        .unwrap();
        drop(conn);

        let store = store(dir.path());
        assert!(store.get_memory_full(2).unwrap().is_none());
        assert_eq!(
            store.get_memory_full(1).unwrap().unwrap().project_path,
            "git:identity"
        );
        assert!(store
            .pull_changefeed("memories", 0, 100)
            .unwrap()
            .rows
            .iter()
            .any(|row| row.module_row_id == 2 && row.op == "tombstone"));
    }

    #[test]
    fn authority_route_binding_schema_30_live_upgrade_rekeys_through_caller_fence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.create_scalar_function(
            "mc_note_caller_project",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_context| Ok(String::new()),
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE cortexkit_schema_version (
                 namespace TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 applied_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (namespace, version)
             );",
        )
        .unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 30)
        {
            conn.execute_batch(migration.statements).unwrap();
            conn.execute(
                "INSERT INTO cortexkit_schema_version(namespace, version, applied_at_unix)
                 VALUES (?1, ?2, 0)",
                params![NS, migration.version],
            )
            .unwrap();
        }
        drop(conn);

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.create_scalar_function(
            "mc_note_caller_project",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_context| Ok("/worktrees/repo".to_string()),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
             VALUES ('/worktrees/repo', 'context', 'git:identity')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
             VALUES ('context', 'git:identity', 'notes', 'MODULE')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mc_notes(id, type, project_path, content, status, harness)
             VALUES (1, 'smart', '/worktrees/repo', 'remember me', 'active', 'module')",
            [],
        )
        .unwrap();
        drop(conn);

        let store = store(dir.path());
        let versions = store
            .inner
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT version FROM cortexkit_schema_version
                     WHERE namespace = ?1 ORDER BY version",
                )?;
                let versions = statement
                    .query_map(params![NS], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(versions)
            })
            .unwrap();
        assert_eq!(versions, (1_i64..=37).collect::<Vec<_>>());
        assert_eq!(
            store
                .get_note_by_id("git:identity", "session", 1)
                .unwrap()
                .unwrap()
                .project_path,
            "git:identity"
        );
        let feed = store.pull_changefeed("notes", 0, 100).unwrap();
        assert!(feed.rows.iter().any(|row| {
            row.module_row_id == 1
                && row.op == "update"
                && row
                    .full_row_snapshot
                    .get("project_path")
                    .and_then(Value::as_str)
                    == Some("git:identity")
        }));
    }

    #[test]
    fn authority_route_binding_rekeys_twins_that_arrive_after_the_first_bind() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let route_project_root = "/worktrees/repo";
        let identity = "git:identity";
        store
            .bind_authority_route("context", identity, route_project_root)
            .unwrap();
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_memories
                         (id, project_path, category, content, normalized_hash, importance, status,
                          first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (1, ?1, 'CONSTRAINTS', 'same fact', 'same-hash', 50, 'active', 0, 0, 0, 0)",
                    params![identity],
                )?;
                tx.execute(
                    "INSERT INTO mc_memories
                         (id, project_path, category, content, normalized_hash, importance, status,
                          first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (2, ?1, 'CONSTRAINTS', 'same fact', 'same-hash', 50, 'active', 0, 0, 0, 0)",
                    params![route_project_root],
                )?;
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES ('context', ?1, 'memories', 'MODULE')",
                    params![identity],
                )?;
                Ok(())
            })
            .unwrap();

        // Repeating the bind repairs duplicates created when the binding was stored before
        // either corresponding memory row existed; bind-time cleanup can then remove the
        // duplicate row and preserve the canonical identity regardless of write order.
        store
            .bind_authority_route("context", identity, route_project_root)
            .unwrap();

        assert!(store.get_memory_full(2).unwrap().is_none());
        assert_eq!(
            store.get_memory_full(1).unwrap().unwrap().project_path,
            identity
        );
        let feed = store.pull_changefeed("memories", 0, 100).unwrap();
        assert!(feed
            .rows
            .iter()
            .any(|row| row.module_row_id == 2 && row.op == "tombstone"));
    }

    #[test]
    fn authority_state_sync_adopts_seed_identity_instead_of_inserting_a_twin() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let route_project_root = "/worktrees/repo";
        let identity = "git:identity";
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES ('context', ?1, 'memories', 'MODULE')",
                    params![identity],
                )?;
                tx.execute(
                    "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
                     VALUES (?1, 'context', ?2)",
                    params![route_project_root, identity],
                )?;
                tx.execute(
                    "INSERT INTO mc_memories
                         (id, project_path, category, content, normalized_hash, importance, status,
                          first_seen_at, created_at, updated_at, last_seen_at,
                          context_store_uuid, context_row_id)
                     VALUES (8214, ?1, 'CONFIG_VALUES', 'drive model', 'same-hash', 50, 'active',
                             0, 0, 0, 0, 'context', 9395)",
                    params![identity],
                )?;
                Ok(())
            })
            .unwrap();
        let mut incoming = memory(9395, "drive model");
        incoming.project_path = identity.to_string();
        incoming.category = "CONFIG_VALUES".to_string();
        incoming.normalized_hash = "same-hash".to_string();

        store
            .apply_authority_state_sync(ShadowStateSyncRequest {
                session_id: "authority-session",
                shadow_project_path: route_project_root,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[incoming],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();

        let rows = store
            .inner
            .with_conn(|conn| {
                let mut statement = conn.prepare(
                    "SELECT id, project_path, context_store_uuid, context_row_id
                       FROM mc_memories
                      WHERE category = 'CONFIG_VALUES' AND normalized_hash = 'same-hash'
                      ORDER BY id",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                8214,
                identity.to_string(),
                Some("context".to_string()),
                Some(9395)
            )]
        );
    }

    #[test]
    fn authority_state_sync_fences_module_owned_memory_rows() {
        for authority_state in ["PREPARING", "MODULE", "DRAINING"] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(dir.path());
            let route_project_root = "/worktrees/repo";
            let identity = "git:identity";
            store
                .inner
                .with_conn_fenced(|tx| {
                    tx.execute(
                        "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                         VALUES ('context', ?1, 'memories', ?2)",
                        params![identity, authority_state],
                    )?;
                    tx.execute(
                        "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
                         VALUES (?1, 'context', ?2)",
                        params![route_project_root, identity],
                    )?;
                    tx.execute(
                        "INSERT INTO mc_memories
                             (id, project_path, category, content, normalized_hash, importance, status,
                              first_seen_at, created_at, updated_at, last_seen_at, classified_at,
                              context_store_uuid, context_row_id)
                         VALUES (8214, ?1, 'CONFIG_VALUES', 'module fact', 'same-hash', 85, 'active',
                                 0, 0, 0, 0, 1234, 'context', 9395)",
                        params![identity],
                    )?;
                    Ok(())
                })
                .unwrap();

            let mut incoming = memory(9395, "module fact");
            incoming.project_path = identity.to_string();
            incoming.category = "CONFIG_VALUES".to_string();
            incoming.normalized_hash = "same-hash".to_string();
            incoming.importance = Some(50);
            incoming.classified_at = None;

            let result = store
                .apply_authority_state_sync(ShadowStateSyncRequest {
                    session_id: "authority-session",
                    shadow_project_path: route_project_root,
                    shadow_generation: 0,
                    expected_shadow_seq: 0,
                    seed_boundary_id: None,
                    drop_seeds: &[],
                    drop_seed_skipped: 0,
                    compartments: &[],
                    memories: &[incoming],
                    memory_mutations: &[],
                    user_profile: &[],
                    workspace: None,
                    last_todo_state: None,
                    project_memory_epoch: None,
                    user_profile_version: None,
                    pending_agent_drops: &[],
                    pending_agent_drops_skipped: 0,
                    user_hint_seeds: &[],
                    auto_search_hint_skipped: 0,
                    note_nudge_anchors: None,
                    todo_synthetic_anchor: None,
                    todo_synthetic_anchor_present: false,
                    emergency_latches: None,
                    acked_watermarks: serde_json::Value::Null,
                })
                .unwrap();

            assert!(
                result.memories_skipped,
                "state {authority_state} must fence TS rows"
            );
            let row = store
                .inner
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT importance, classified_at FROM mc_memories WHERE id = 8214",
                        [],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
                    )
                })
                .unwrap();
            assert_eq!(row, (85, Some(1234)));
        }
    }

    #[test]
    fn authority_state_sync_replaces_memory_rows_for_ts_and_unbound_routes() {
        for authority_state in [None, Some("TS")] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(dir.path());
            let route_project_root = "/worktrees/repo";
            let identity = "git:identity";
            store
                .inner
                .with_conn_fenced(|tx| {
                    if let Some(state) = authority_state {
                        tx.execute(
                            "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                             VALUES ('context', ?1, 'memories', ?2)",
                            params![identity, state],
                        )?;
                        tx.execute(
                            "INSERT INTO mc_authority_route_bindings(route_project_root, context_store_uuid, project)
                             VALUES (?1, 'context', ?2)",
                            params![route_project_root, identity],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO mc_memories
                             (id, project_path, category, content, normalized_hash, importance, status,
                              first_seen_at, created_at, updated_at, last_seen_at, classified_at,
                              context_store_uuid, context_row_id)
                         VALUES (8214, ?1, 'CONFIG_VALUES', 'module fact', 'same-hash', 85, 'active',
                                 0, 0, 0, 0, 1234, 'context', 9395)",
                        params![identity],
                    )?;
                    Ok(())
                })
                .unwrap();

            let mut incoming = memory(
                if authority_state.is_some() {
                    9395
                } else {
                    8214
                },
                "updated fact",
            );
            incoming.project_path = identity.to_string();
            incoming.category = "CONFIG_VALUES".to_string();
            incoming.normalized_hash = "updated-hash".to_string();
            incoming.importance = Some(50);
            incoming.classified_at = None;

            let result = store
                .apply_authority_state_sync(ShadowStateSyncRequest {
                    session_id: "authority-session",
                    shadow_project_path: route_project_root,
                    shadow_generation: 0,
                    expected_shadow_seq: 0,
                    seed_boundary_id: None,
                    drop_seeds: &[],
                    drop_seed_skipped: 0,
                    compartments: &[],
                    memories: &[incoming],
                    memory_mutations: &[],
                    user_profile: &[],
                    workspace: None,
                    last_todo_state: None,
                    project_memory_epoch: None,
                    user_profile_version: None,
                    pending_agent_drops: &[],
                    pending_agent_drops_skipped: 0,
                    user_hint_seeds: &[],
                    auto_search_hint_skipped: 0,
                    note_nudge_anchors: None,
                    todo_synthetic_anchor: None,
                    todo_synthetic_anchor_present: false,
                    emergency_latches: None,
                    acked_watermarks: serde_json::Value::Null,
                })
                .unwrap();

            assert!(!result.memories_skipped);
            let row = store
                .inner
                .with_conn(|conn| {
                    conn.query_row(
                        "SELECT content, importance, classified_at FROM mc_memories WHERE id = 8214",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, Option<i64>>(2)?,
                            ))
                        },
                    )
                })
                .unwrap();
            assert_eq!(row, ("updated fact".to_string(), 50, None));
            assert!(store
                .pull_changefeed("memories", 0, 100)
                .unwrap()
                .rows
                .iter()
                .any(|row| {
                    row.op == "update" && row.full_row_snapshot["classified_at"].is_null()
                }));
        }
    }

    #[test]
    fn authority_route_binding_rejects_path_vocabulary_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let route_project_root = "/worktrees/repo";
        let identity = "git:identity";
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES ('context', ?1, 'memories', 'MODULE')",
                    params![identity],
                )?;
                Ok(())
            })
            .unwrap();
        store
            .bind_authority_route("context", identity, route_project_root)
            .unwrap();

        let error = store
            .insert_memory(InsertMemoryInput {
                project_path: route_project_root,
                route_project_root: Some(route_project_root),
                category: "CONSTRAINTS",
                content: "must not split",
                source_session_id: None,
                source_type: Some("agent"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: 1,
            })
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(identity));
        assert!(message.contains(route_project_root));
    }

    #[test]
    fn authority_feed_triggers_cover_idempotency_and_null_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let memory_id = store
            .insert_memory(InsertMemoryInput {
                project_path: "feed-project",
                route_project_root: None,
                category: "CONSTRAINTS",
                content: "first",
                source_session_id: None,
                source_type: Some("tool"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: 1,
            })
            .unwrap();
        let trigger_sql: String = store
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT group_concat(sql, char(10)) FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'mc_%_feed_%'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert!(!trigger_sql.contains("!="));
        assert!(trigger_sql.contains(" IS NOT "));

        let initial = store.pull_changefeed("memories", 0, 100).unwrap();
        assert_eq!(initial.rows.len(), 1);
        assert_eq!(initial.rows[0].op, "insert");
        assert_eq!(
            initial.rows[0].content_hash.as_deref(),
            initial.rows[0].full_row_snapshot["normalized_hash"].as_str()
        );

        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "UPDATE mc_memories SET content = content WHERE id = ?1",
                    params![memory_id],
                )?;
                tx.execute(
                    "UPDATE mc_memories SET last_retrieved_at = 1 WHERE id = ?1",
                    params![memory_id],
                )?;
                tx.execute(
                    "UPDATE mc_memories SET last_retrieved_at = NULL WHERE id = ?1",
                    params![memory_id],
                )?;
                tx.execute("DELETE FROM mc_memories WHERE id = ?1", params![memory_id])?;
                Ok(())
            })
            .unwrap();
        let page = store
            .pull_changefeed("memories", initial.next_cursor, 100)
            .unwrap();
        assert_eq!(
            page.rows.len(),
            3,
            "no-op updates must not append feed rows"
        );
        assert_eq!(page.rows[0].op, "update");
        assert_eq!(page.rows[1].op, "update");
        assert_eq!(page.rows[2].op, "tombstone");

        let classified_id = store
            .insert_memory(InsertMemoryInput {
                project_path: "feed-project",
                route_project_root: None,
                category: "CONSTRAINTS",
                content: "classified fact",
                source_session_id: None,
                source_type: Some("dreamer"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: 2,
            })
            .unwrap();
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_authority(context_store_uuid, project, domain, state)
                     VALUES ('store-uuid', 'feed-project', 'memories', 'MODULE')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let classified_hash = store
            .get_memory_full(classified_id)
            .unwrap()
            .unwrap()
            .normalized_hash;
        let classification_start = store
            .pull_changefeed("memories", page.next_cursor, 100)
            .unwrap()
            .next_cursor;
        store
            .set_memory_classification(
                "store-uuid",
                "feed-project",
                0,
                &[ClassificationUpdate {
                    memory_id: classified_id,
                    content_hash_at_prompt: classified_hash,
                    importance: Some(77),
                    scope: None,
                    shareable: None,
                }],
                4242,
            )
            .unwrap();
        let classification_page = store
            .pull_changefeed("memories", classification_start, 100)
            .unwrap();
        assert_eq!(classification_page.rows.len(), 1);
        assert_eq!(classification_page.rows[0].op, "update");
        assert_eq!(
            classification_page.rows[0].full_row_snapshot["classified_at"],
            serde_json::json!(4242)
        );

        let note = serde_json::json!({
            "id": 12,
            "project_path": "feed-project",
            "session_id": "session",
            "content": "note",
            "status": "active",
            "created_at_ms": 1,
            "updated_at_ms": 1
        });
        let first_id = store
            .seed_authority_row("store-uuid", "notes", 12, &note)
            .unwrap();
        let second_id = store
            .seed_authority_row("store-uuid", "notes", 12, &note)
            .unwrap();
        assert_eq!(first_id, second_id, "seed retries use the source key");
        let notes = store.pull_changefeed("notes", 0, 100).unwrap();
        assert_eq!(notes.rows.len(), 1, "idempotent same-row seed is a no-op");
        assert_eq!(notes.rows[0].op, "insert");
    }

    #[test]
    fn authority_seed_resolves_forward_memory_references_without_raw_id_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let snapshot = |source_id: i64, superseded_by: Option<i64>| {
            serde_json::json!({
                "id": source_id,
                "project_path": "project",
                "category": "CONSTRAINTS",
                "content": format!("memory {source_id}"),
                "normalized_hash": format!("h{source_id}"),
                "status": "active",
                "superseded_by_memory_id": superseded_by,
            })
        };
        let source = store
            .seed_authority_row("store-uuid", "memories", 100, &snapshot(100, Some(200)))
            .unwrap();
        assert_eq!(
            store
                .get_memory_full(source)
                .unwrap()
                .unwrap()
                .superseded_by_memory_id,
            None,
            "an unresolved context.db id must never be stored as a module id"
        );
        let target = store
            .seed_authority_row("store-uuid", "memories", 200, &snapshot(200, None))
            .unwrap();
        assert_eq!(
            store
                .get_memory_full(source)
                .unwrap()
                .unwrap()
                .superseded_by_memory_id,
            Some(target)
        );
    }

    #[test]
    fn authority_state_machine_persists_generations_and_drain_journal() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .bind_authority_route("store-uuid", "project", "/repo")
            .unwrap();
        let preparing = store
            .authority_begin_prepare("store-uuid", "project", "memories")
            .unwrap();
        assert_eq!(preparing.state, "PREPARING");
        assert_eq!(preparing.generation, 1);
        assert_eq!(
            store
                .authority_project_for_route("/repo", "memories")
                .unwrap(),
            None,
            "PREPARING must keep transforms on the complete TypeScript snapshot"
        );
        let module = store
            .authority_finish_prepare(
                "store-uuid",
                "project",
                "memories",
                preparing.generation,
                "hash",
                "hash",
                true,
            )
            .unwrap();
        assert_eq!(module.state, "MODULE");
        assert_eq!(module.generation, 2);
        assert_eq!(
            store
                .authority_project_for_route("/repo", "memories")
                .unwrap()
                .as_deref(),
            Some("project")
        );
        let draining = store
            .authority_begin_drain("store-uuid", "project", "memories", "lease", 100, 0)
            .unwrap();
        assert_eq!(draining.state, "DRAINING");
        assert_eq!(draining.captured_upper_bound, Some(0));
        assert_eq!(
            store
                .authority_project_for_route("/repo", "memories")
                .unwrap()
                .as_deref(),
            Some("project")
        );
        let token = draining.coordinator_token.clone().expect("token minted");
        let stepped = store
            .authority_drain_step(
                "store-uuid",
                "project",
                "memories",
                draining.generation,
                "seed",
                Some(0),
                &token,
                0,
            )
            .unwrap();
        assert!(stepped.step_seed);
    }

    #[test]
    fn authority_drain_begin_resumes_each_crash_journal_position() {
        for completed_steps in [0usize, 3, 6] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(dir.path());
            let preparing = store
                .authority_begin_prepare("store-uuid", "project", "memories")
                .unwrap();
            store
                .authority_finish_prepare(
                    "store-uuid",
                    "project",
                    "memories",
                    preparing.generation,
                    "hash",
                    "hash",
                    true,
                )
                .unwrap();
            let draining = store
                .authority_begin_drain("store-uuid", "project", "memories", "coordinator", 100, 0)
                .unwrap();
            let token = draining.coordinator_token.clone().expect("token minted");
            let steps = [
                "seed",
                "memories",
                "notes",
                "compartments",
                "reconcile",
                "verify",
            ];
            for step in steps.iter().take(completed_steps) {
                store
                    .authority_drain_step(
                        "store-uuid",
                        "project",
                        "memories",
                        draining.generation,
                        step,
                        Some(0),
                        &token,
                        0,
                    )
                    .unwrap();
            }

            let resumed = store
                .authority_begin_drain("store-uuid", "project", "memories", "coordinator", 200, 101)
                .unwrap();
            assert_eq!(resumed.generation, draining.generation);
            assert_eq!(resumed.captured_upper_bound, draining.captured_upper_bound);
            let resume_token = resumed.coordinator_token.clone().expect("resume token");
            assert_ne!(
                resume_token, token,
                "resume mints a fresh coordinator token"
            );
            for step in steps.iter().skip(completed_steps) {
                store
                    .authority_drain_step(
                        "store-uuid",
                        "project",
                        "memories",
                        resumed.generation,
                        step,
                        Some(0),
                        &resume_token,
                        101,
                    )
                    .unwrap();
            }
            let finished = store
                .authority_finish_drain(
                    "store-uuid",
                    "project",
                    "memories",
                    resumed.generation,
                    "hash",
                    "hash",
                    true,
                    &resume_token,
                    101,
                )
                .unwrap();
            assert_eq!(finished.state, "TS");
        }
    }

    #[test]
    fn authority_finish_drain_fences_and_recaptures_a_late_feed_append() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let preparing = store
            .authority_begin_prepare("ctx", "project", "memories")
            .unwrap();
        let checksum = store
            .authority_seed_checksum("ctx", "project", "memories")
            .unwrap();
        store
            .authority_verify_prepare(
                "ctx",
                "project",
                "memories",
                preparing.generation,
                &checksum,
                &checksum,
            )
            .unwrap();
        store
            .authority_ack_prepare("ctx", "project", "memories", preparing.generation)
            .unwrap();
        store
            .bind_authority_route("ctx", "project", "/route")
            .unwrap();
        let draining = store
            .authority_begin_drain("ctx", "project", "memories", "lease", 10_000, 1)
            .unwrap();
        let token = draining.coordinator_token.as_deref().unwrap();
        for step in [
            "seed",
            "memories",
            "notes",
            "compartments",
            "reconcile",
            "verify",
        ] {
            store
                .authority_drain_step(
                    "ctx",
                    "project",
                    "memories",
                    draining.generation,
                    step,
                    Some(0),
                    token,
                    2,
                )
                .unwrap();
        }
        let rejected = store.with_facade_mutation("/route", "memories", || {
            store.insert_memory(InsertMemoryInput {
                project_path: "project",
                route_project_root: Some("/route"),
                category: "CONSTRAINTS",
                content: "rejected",
                source_session_id: None,
                source_type: Some("tool"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: 3,
            })
        });
        assert!(rejected
            .unwrap_err()
            .to_string()
            .contains("authority_draining"));
        assert_eq!(
            store
                .pull_changefeed("memories", 0, 100)
                .unwrap()
                .next_cursor,
            0
        );

        store
            .with_facade_mutation("/route", "memories", || {
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            store.insert_memory(InsertMemoryInput {
                                project_path: "project",
                                route_project_root: None,
                                category: "CONSTRAINTS",
                                content: "late",
                                source_session_id: None,
                                source_type: Some("tool"),
                                importance: Some(50),
                                expires_at: None,
                                metadata_json: None,
                                now_ms: 3,
                            })
                        })
                        .join()
                        .unwrap()
                })
            })
            .unwrap();
        assert!(matches!(
            store.authority_finish_drain(
                "ctx",
                "project",
                "memories",
                draining.generation,
                "same",
                "same",
                true,
                token,
                3,
            ),
            Err(McStoreError::AuthorityFeedHeadAdvanced {
                captured: 0,
                found: 1
            })
        ));

        let recaptured = store
            .authority_begin_drain("ctx", "project", "memories", "lease", 10_000, 4)
            .unwrap();
        assert_eq!(recaptured.captured_upper_bound, Some(1));
        let finished = store
            .authority_finish_drain(
                "ctx",
                "project",
                "memories",
                recaptured.generation,
                "same",
                "same",
                true,
                recaptured.coordinator_token.as_deref().unwrap(),
                5,
            )
            .unwrap();
        assert_eq!(finished.state, "TS");
    }

    #[test]
    fn authority_drain_resume_rejects_a_different_live_lease() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let preparing = store
            .authority_begin_prepare("store-uuid", "project", "memories")
            .unwrap();
        store
            .authority_finish_prepare(
                "store-uuid",
                "project",
                "memories",
                preparing.generation,
                "hash",
                "hash",
                true,
            )
            .unwrap();
        store
            .authority_begin_drain("store-uuid", "project", "memories", "first", 200, 100)
            .unwrap();
        assert!(store
            .authority_begin_drain("store-uuid", "project", "memories", "second", 250, 150)
            .is_err());
        let resumed = store
            .authority_begin_drain("store-uuid", "project", "memories", "second", 400, 201)
            .unwrap();
        assert_eq!(resumed.coordinator_lease.as_deref(), Some("second"));
    }

    #[test]
    fn authority_drain_stale_coordinator_token_rejected_after_takeover() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let preparing = store
            .authority_begin_prepare("store-uuid", "project", "memories")
            .unwrap();
        store
            .authority_finish_prepare(
                "store-uuid",
                "project",
                "memories",
                preparing.generation,
                "hash",
                "hash",
                true,
            )
            .unwrap();
        let first = store
            .authority_begin_drain("store-uuid", "project", "memories", "first", 100, 0)
            .unwrap();
        let stale_token = first.coordinator_token.clone().expect("first token");
        let second = store
            .authority_begin_drain("store-uuid", "project", "memories", "second", 400, 101)
            .unwrap();
        let live_token = second.coordinator_token.clone().expect("second token");
        assert_ne!(stale_token, live_token);
        assert!(store
            .authority_drain_step(
                "store-uuid",
                "project",
                "memories",
                second.generation,
                "seed",
                Some(0),
                &stale_token,
                150,
            )
            .is_err());
        assert!(store
            .authority_finish_drain(
                "store-uuid",
                "project",
                "memories",
                second.generation,
                "hash",
                "hash",
                true,
                &stale_token,
                150,
            )
            .is_err());
        store
            .authority_drain_step(
                "store-uuid",
                "project",
                "memories",
                second.generation,
                "seed",
                Some(0),
                &live_token,
                150,
            )
            .unwrap();
    }

    #[test]
    fn authority_pending_references_are_project_scoped_and_block_complete() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let snapshot = |project: &str, source_id: i64, superseded_by: Option<i64>| {
            serde_json::json!({
                "id": source_id,
                "project_path": project,
                "category": "CONSTRAINTS",
                "content": format!("{project}-{source_id}"),
                "normalized_hash": format!("h{source_id}"),
                "status": "active",
                "superseded_by_memory_id": superseded_by,
            })
        };
        store
            .authority_begin_prepare("store-uuid", "project-a", "memories")
            .unwrap();
        store
            .seed_authority_row(
                "store-uuid",
                "memories",
                1,
                &snapshot("project-a", 1, Some(99)),
            )
            .unwrap();
        store
            .authority_begin_prepare("store-uuid", "project-b", "memories")
            .unwrap();
        let pending_b: i64 = store
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM mc_authority_pending_memory_references
                      WHERE context_store_uuid = 'store-uuid' AND project = 'project-a'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(
            pending_b, 1,
            "project B begin must not wipe project A pending refs"
        );
        store
            .seed_authority_row(
                "store-uuid",
                "memories",
                2,
                &snapshot("project-b", 2, Some(88)),
            )
            .unwrap();
        let preparing_b = store
            .authority_status("store-uuid", "project-b", "memories")
            .unwrap()
            .unwrap();
        let err = store
            .authority_verify_prepare(
                "store-uuid",
                "project-b",
                "memories",
                preparing_b.generation,
                "x",
                "x",
            )
            .expect_err("unresolved pending refs must reject complete");
        assert!(
            err.to_string()
                .contains("unresolved pending memory references")
                || format!("{err:?}").contains("UnresolvedPending")
                || format!("{err}").contains("pending"),
            "typed pending-ref rejection, got {err}"
        );
    }

    #[test]
    fn natural_foreign_expiry_does_not_change_workspace_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let own = "git:exp-own";
        let foreign = "git:exp-foreign";
        let deadline = 1_000_i64;
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'exp-ws','[\"ARCHITECTURE\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at) VALUES (1, ?1, ?1, ?1, 0), (1, ?2, ?2, ?2, 0)",
                    params![own, foreign],
                )?;
                tx.execute(
                    "INSERT INTO mc_memories
                       (id, project_path, category, content, normalized_hash, importance,
                        scope, shareable, status, expires_at, first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (1, ?1, 'ARCHITECTURE', 'shared until expiry', 'h1', 50,
                             'project', 1, 'active', ?2, 0, 0, 0, 0)",
                    params![foreign, deadline],
                )?;
                Ok(())
            })
            .unwrap();
        let before = store.workspace_fingerprint(own, deadline - 1).unwrap();
        let at_deadline = store.workspace_fingerprint(own, deadline).unwrap();
        let after = store.workspace_fingerprint(own, deadline + 1).unwrap();
        assert_eq!(
            before, at_deadline,
            "crossing expiry must not change identity"
        );
        assert_eq!(
            at_deadline, after,
            "fingerprint remains stable while expiry is handled by the next natural HARD"
        );
    }

    #[test]
    fn same_second_visibility_revocation_does_not_bump_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let own = "git:same-sec-own";
        let foreign = "git:same-sec-foreign";
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'ss-ws','[\"ARCHITECTURE\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at) VALUES (1, ?1, ?1, ?1, 0), (1, ?2, ?2, ?2, 0)",
                    params![own, foreign],
                )?;
                tx.execute(
                    "INSERT INTO mc_memories
                       (id, project_path, category, content, normalized_hash, importance,
                        scope, shareable, status, expires_at, first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (1, ?1, 'ARCHITECTURE', 'shared', 'h1', 50,
                             'project', 1, 'active',
                             CAST(strftime('%s', 'now') AS INTEGER) * 1000, 0, 0, 0, 0)",
                    params![foreign],
                )?;
                // Revoke while expires_at equals SQLite's second clock — the old trigger
                // could treat OLD as already invisible and skip the epoch bump.
                tx.execute("UPDATE mc_memories SET shareable = 0 WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap();
        let epoch = store
            .inner
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT epoch FROM mc_memory_visibility_epoch WHERE project_path = ?1",
                    params![foreign],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            })
            .unwrap();
        assert_eq!(epoch, None);
    }

    #[test]
    fn authority_seq_fence_rejects_an_interleaved_stale_sender() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let session = "authority-session";
        let project = "authority-project";

        store
            .apply_authority_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: project,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[comp(0, 0, "first#0")],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: Some("first".to_string()),
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::json!({"sender": "first"}),
            })
            .unwrap();

        let stale = store
            .apply_authority_state_sync(ShadowStateSyncRequest {
                session_id: session,
                shadow_project_path: project,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[comp(1, 1, "second#0")],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: Some("second".to_string()),
                project_memory_epoch: None,
                user_profile_version: None,
                pending_agent_drops: &[],
                pending_agent_drops_skipped: 0,
                user_hint_seeds: &[],
                auto_search_hint_skipped: 0,
                note_nudge_anchors: None,
                todo_synthetic_anchor: None,
                todo_synthetic_anchor_present: false,
                emergency_latches: None,
                acked_watermarks: serde_json::json!({"sender": "second"}),
            })
            .unwrap_err();

        assert!(matches!(
            stale,
            ShadowStateSyncError::AuthoritySeqMismatch {
                expected: 0,
                found: 1
            }
        ));
        let loaded = store.load(session).unwrap();
        assert_eq!(loaded.meta.shadow_seq, 1);
        assert_eq!(loaded.meta.last_todo_state.as_deref(), Some("first"));
        assert_eq!(
            store.load_compartments(session).unwrap()[0].end_message_id,
            "first#0"
        );
    }

    #[test]
    fn set_memory_verification_and_mapping_fence_rows_and_append_feed_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let prepared = store
            .authority_begin_prepare("context", "git:applies", "memories")
            .unwrap();
        let authority = store
            .authority_finish_prepare(
                "context",
                "git:applies",
                "memories",
                prepared.generation,
                "digest",
                "digest",
                true,
            )
            .unwrap();
        store
            .seed_memory(1, "git:applies", "CONSTRAINTS", "one", 1)
            .unwrap();
        store
            .seed_memory(2, "git:applies", "CONSTRAINTS", "two", 1)
            .unwrap();
        store
            .seed_memory(3, "git:other", "CONSTRAINTS", "three", 1)
            .unwrap();
        let hash = |id| store.get_memory_full(id).unwrap().unwrap().normalized_hash;
        let classified = store
            .set_memory_classification(
                "context",
                "git:applies",
                authority.generation,
                &[ClassificationUpdate {
                    memory_id: 1,
                    content_hash_at_prompt: hash(1),
                    importance: Some(88),
                    scope: Some("project".into()),
                    shareable: Some(false),
                }],
                0,
            )
            .unwrap();
        assert_eq!(classified.accepted, vec![1]);
        let result = store
            .set_memory_verification(
                "context",
                "git:applies",
                authority.generation,
                &[
                    VerificationUpdate {
                        memory_id: 1,
                        content_hash_at_prompt: hash(1),
                        verification_status: "verified".into(),
                        updated_content: None,
                        archive_reason: None,
                    },
                    VerificationUpdate {
                        memory_id: 2,
                        content_hash_at_prompt: "stale".into(),
                        verification_status: "verified".into(),
                        updated_content: None,
                        archive_reason: None,
                    },
                    VerificationUpdate {
                        memory_id: 99,
                        content_hash_at_prompt: "missing".into(),
                        verification_status: "verified".into(),
                        updated_content: None,
                        archive_reason: None,
                    },
                    VerificationUpdate {
                        memory_id: 3,
                        content_hash_at_prompt: hash(3),
                        verification_status: "verified".into(),
                        updated_content: None,
                        archive_reason: None,
                    },
                ],
                0,
            )
            .unwrap();
        assert_eq!(result.accepted, vec![1]);
        assert_eq!(
            result
                .rejected
                .iter()
                .map(|row| row.reason.as_str())
                .collect::<Vec<_>>(),
            vec!["stale", "not_found", "not_owned"]
        );
        let updated = store
            .set_memory_verification(
                "context",
                "git:applies",
                authority.generation,
                &[VerificationUpdate {
                    memory_id: 2,
                    content_hash_at_prompt: hash(2),
                    verification_status: "update".into(),
                    updated_content: Some("two changed".into()),
                    archive_reason: None,
                }],
                2,
            )
            .unwrap();
        assert_eq!(updated.accepted, vec![2]);
        let archived = store
            .set_memory_verification(
                "context",
                "git:applies",
                authority.generation,
                &[VerificationUpdate {
                    memory_id: 1,
                    content_hash_at_prompt: hash(1),
                    verification_status: "archive".into(),
                    updated_content: None,
                    archive_reason: Some("obsolete".into()),
                }],
                3,
            )
            .unwrap();
        assert_eq!(archived.accepted, vec![1]);
        let mapping = store
            .set_memory_mapping(
                "context",
                "git:applies",
                authority.generation,
                &[MappingUpdate {
                    memory_id: 2,
                    content_hash_at_prompt: hash(2),
                    mapped_files: Some(vec!["src/lib.rs".into()]),
                }],
                4,
            )
            .unwrap();
        assert_eq!(mapping.accepted, vec![2]);
        assert!(store
            .pull_changefeed("memories", 0, 100)
            .unwrap()
            .rows
            .iter()
            .any(|row| row.full_row_snapshot.get("mapping").is_some()));
        assert_memory_feed_snapshots_complete(&store);

        // The live-snapshot arm feeds the mirror resnapshot healer, so its rows must
        // satisfy the same completeness invariant as changefeed emissions — a reduced
        // projection here would null-clobber the very columns the heal exists to repair.
        let live = store.pull_live_memory_snapshot(0, 100).unwrap();
        assert!(!live.rows.is_empty());
        for row in &live.rows {
            let object = row.full_row_snapshot.as_object().unwrap();
            for column in MEMORY_FEED_COLUMNS {
                assert!(
                    object.contains_key(*column),
                    "live snapshot omitted column {column}"
                );
            }
        }
        assert!(live
            .rows
            .iter()
            .any(|row| row.full_row_snapshot["source_type"].as_str().is_some()));
    }
}
