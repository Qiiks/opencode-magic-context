//! Historian writer orchestration: the durable firing state machine
//! (idle → firing → awaiting_producer → validating → publishing), the pinned
//! ordinal-range chunk snapshot with fail-loud fingerprint verification, and the
//! CAS-gated publish transaction whose writes surface only through the m1
//! watermark on the next materializing pass (a publish never mutates cached
//! render state directly).

use std::fmt;

use mc_store::{
    FactCandidate, HistorianChunkRange, HistorianDurableState, HistorianPhase,
    HistorianPublishError, HistorianPublishPredicate, HistorianPublishRequest,
    HistorianPublishResult, McStore, McStoreError, StoredCompartment,
};

/// A validated historian chunk ready for durable publication. The sibling
/// validation module owns parsing and semantic checks; orchestration only needs
/// the additive rows plus the first ordinal left unprocessed by this chunk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidatedChunk {
    pub compartments: Vec<StoredCompartment>,
    pub facts: Vec<FactCandidate>,
    pub unprocessed_from: Option<u64>,
}

/// One flat item in the pinned chunk snapshot used to guard producer output.
/// The fingerprint intentionally records byte lengths rather than content bytes:
/// content edits, insertion/removal, and type/id changes alter the fingerprint,
/// while unrelated metadata drift cannot stale a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSnapshotItem<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub bytes: &'a str,
}

/// Compute the content-stable historian chunk fingerprint. For already-flattened
/// chunk items it uses ordered `(id, kind, byte-length)` pieces joined without
/// hashing so mismatches are readable in diagnostics.
pub fn compute_chunk_fingerprint(items: &[ChunkSnapshotItem<'_>]) -> String {
    items
        .iter()
        .map(|item| format!("{}:{}:{}", item.id, item.kind, item.bytes.len()))
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    Fired(HistorianDurableState),
    Busy(HistorianDurableState),
}

#[derive(Debug)]
pub enum HistorianStateError {
    InvalidRange {
        from_ordinal: u64,
        to_ordinal: u64,
    },
    InvalidTransition {
        from: HistorianPhase,
        event: &'static str,
    },
    MissingProducerIds {
        firing_seq: u64,
    },
    FingerprintMismatch {
        expected: String,
        found: String,
    },
    Store(McStoreError),
    Publish(HistorianPublishError),
}

impl fmt::Display for HistorianStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistorianStateError::InvalidRange {
                from_ordinal,
                to_ordinal,
            } => write!(
                f,
                "historian invalid chunk range: from {from_ordinal} is after to {to_ordinal}"
            ),
            HistorianStateError::InvalidTransition { from, event } => write!(
                f,
                "historian invalid transition: event {event} cannot run from {}",
                from.as_str()
            ),
            HistorianStateError::MissingProducerIds { firing_seq } => write!(
                f,
                "historian firing {firing_seq} is missing producer ids needed for reattach/publish"
            ),
            HistorianStateError::FingerprintMismatch { expected, found } => write!(
                f,
                "historian chunk fingerprint mismatch: expected {expected}, found {found}"
            ),
            HistorianStateError::Store(e) => write!(f, "store: {e}"),
            HistorianStateError::Publish(e) => write!(f, "publish: {e}"),
        }
    }
}

impl std::error::Error for HistorianStateError {}

impl From<McStoreError> for HistorianStateError {
    fn from(e: McStoreError) -> Self {
        HistorianStateError::Store(e)
    }
}

impl From<HistorianPublishError> for HistorianStateError {
    fn from(e: HistorianPublishError) -> Self {
        HistorianStateError::Publish(e)
    }
}

/// Try to start a historian firing. Single-flight is enforced here: any
/// non-idle phase returns `Busy` with the unchanged state.
pub fn fire(
    current: &HistorianDurableState,
    from_ordinal: u64,
    to_ordinal: u64,
    chunk_fingerprint: String,
    fired_at_ms: i64,
) -> Result<FireOutcome, HistorianStateError> {
    if from_ordinal > to_ordinal {
        return Err(HistorianStateError::InvalidRange {
            from_ordinal,
            to_ordinal,
        });
    }
    if current.state != HistorianPhase::Idle {
        return Ok(FireOutcome::Busy(current.clone()));
    }

    Ok(FireOutcome::Fired(HistorianDurableState {
        state: HistorianPhase::Firing,
        firing_seq: current.firing_seq.saturating_add(1),
        chunk_range: Some(HistorianChunkRange {
            from_ordinal,
            to_ordinal,
        }),
        chunk_fingerprint,
        producer_session_id: None,
        producer_run_id: None,
        fired_at_ms: Some(fired_at_ms),
        failure_backoff_at_ms: None,
    }))
}

pub fn producer_started(
    current: &HistorianDurableState,
    producer_session_id: String,
    producer_run_id: String,
) -> Result<HistorianDurableState, HistorianStateError> {
    require_phase(current, HistorianPhase::Firing, "producer_started")?;
    let mut next = current.clone();
    next.state = HistorianPhase::AwaitingProducer;
    next.producer_session_id = Some(producer_session_id);
    next.producer_run_id = Some(producer_run_id);
    Ok(next)
}

pub fn output_received(
    current: &HistorianDurableState,
    _output_text: &str,
) -> Result<HistorianDurableState, HistorianStateError> {
    require_phase(current, HistorianPhase::AwaitingProducer, "output_received")?;
    let mut next = current.clone();
    next.state = HistorianPhase::Validating;
    Ok(next)
}

pub fn validation_ok(
    current: &HistorianDurableState,
) -> Result<HistorianDurableState, HistorianStateError> {
    require_phase(current, HistorianPhase::Validating, "validation_ok")?;
    let mut next = current.clone();
    next.state = HistorianPhase::Publishing;
    Ok(next)
}

pub fn tx_committed(
    current: &HistorianDurableState,
) -> Result<HistorianDurableState, HistorianStateError> {
    require_phase(current, HistorianPhase::Publishing, "tx_committed")?;
    Ok(idle_after_success(current.firing_seq))
}

pub fn tx_conflict(
    current: &HistorianDurableState,
    failure_backoff_at_ms: i64,
) -> Result<HistorianDurableState, HistorianStateError> {
    require_phase(current, HistorianPhase::Publishing, "tx_conflict")?;
    Ok(abandon(current, failure_backoff_at_ms))
}

/// Release the single-flight lease after any terminal/missing/expired producer,
/// validation rejection, or stale snapshot. The failed firing sequence is kept so
/// the next fire remains monotonic.
pub fn abandon(
    current: &HistorianDurableState,
    failure_backoff_at_ms: i64,
) -> HistorianDurableState {
    HistorianDurableState {
        state: HistorianPhase::Idle,
        firing_seq: current.firing_seq,
        failure_backoff_at_ms: Some(failure_backoff_at_ms),
        ..HistorianDurableState::default()
    }
}

pub fn verify_chunk_fingerprint(expected: &str, observed: &str) -> Result<(), HistorianStateError> {
    if expected == observed {
        Ok(())
    } else {
        Err(HistorianStateError::FingerprintMismatch {
            expected: expected.to_string(),
            found: observed.to_string(),
        })
    }
}

pub fn publish_predicate(
    state: &HistorianDurableState,
) -> Result<HistorianPublishPredicate, HistorianStateError> {
    let Some(producer_run_id) = state.producer_run_id.clone() else {
        return Err(HistorianStateError::MissingProducerIds {
            firing_seq: state.firing_seq,
        });
    };
    Ok(HistorianPublishPredicate {
        firing_seq: state.firing_seq,
        producer_run_id,
        chunk_fingerprint: state.chunk_fingerprint.clone(),
    })
}

pub fn persist_historian_state(
    store: &McStore,
    session_id: &str,
    next_state: HistorianDurableState,
) -> Result<u64, HistorianStateError> {
    let loaded = store.load(session_id)?;
    let mut meta = loaded.meta.clone();
    meta.historian = next_state;
    if meta == loaded.meta {
        return Ok(loaded.row_version.unwrap_or(0));
    }
    Ok(store.commit(session_id, loaded.row_version, &loaded.core, &meta)?)
}

pub struct ValidatedPublishRequest<'a> {
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub expected_row_version: Option<u64>,
    pub predicate: &'a HistorianPublishPredicate,
    pub observed_chunk_fingerprint: &'a str,
    pub validated: &'a ValidatedChunk,
    pub publication_floor_ordinal: u64,
    pub failure_backoff_at_ms: i64,
}

/// Publish after re-checking the chunk fingerprint at the commit point. A mismatch
/// abandons the matching firing before returning the typed error, so a future fire
/// is not blocked by the stale producer.
pub fn publish_validated_chunk(
    store: &McStore,
    request: ValidatedPublishRequest<'_>,
) -> Result<HistorianPublishResult, HistorianStateError> {
    if request.predicate.chunk_fingerprint != request.observed_chunk_fingerprint {
        abandon_matching_run(
            store,
            request.session_id,
            request.predicate,
            request.failure_backoff_at_ms,
        )?;
        return Err(HistorianStateError::FingerprintMismatch {
            expected: request.predicate.chunk_fingerprint.clone(),
            found: request.observed_chunk_fingerprint.to_string(),
        });
    }

    Ok(store.publish_historian_chunk(HistorianPublishRequest {
        session_id: request.session_id,
        expected_row_version: request.expected_row_version,
        predicate: request.predicate,
        project_path: request.project_path,
        compartments: &request.validated.compartments,
        facts: &request.validated.facts,
        publication_floor_ordinal: request.publication_floor_ordinal,
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartAction {
    Done,
    ReattachProducer {
        producer_session_id: String,
        producer_run_id: String,
        firing_seq: u64,
        chunk_fingerprint: String,
    },
    AbandonedAndRefireEligible {
        firing_seq: u64,
    },
}

/// Interpret durable state after process restart. If publish had committed before
/// the crash, the load observes idle and returns `Done`; if it still observes a
/// publishing row, the transaction did not commit, so the stale single-flight is
/// abandoned and a future trigger may refire when eligible.
pub fn handle_restart_load(
    store: &McStore,
    session_id: &str,
    failure_backoff_at_ms: i64,
) -> Result<RestartAction, HistorianStateError> {
    let loaded = store.load(session_id)?;
    let state = loaded.meta.historian.clone();
    match state.state {
        HistorianPhase::Idle => Ok(RestartAction::Done),
        HistorianPhase::AwaitingProducer => {
            let (Some(producer_session_id), Some(producer_run_id)) = (
                state.producer_session_id.clone(),
                state.producer_run_id.clone(),
            ) else {
                let next = abandon(&state, failure_backoff_at_ms);
                persist_historian_state(store, session_id, next)?;
                return Ok(RestartAction::AbandonedAndRefireEligible {
                    firing_seq: state.firing_seq,
                });
            };
            Ok(RestartAction::ReattachProducer {
                producer_session_id,
                producer_run_id,
                firing_seq: state.firing_seq,
                chunk_fingerprint: state.chunk_fingerprint,
            })
        }
        HistorianPhase::Firing | HistorianPhase::Validating | HistorianPhase::Publishing => {
            let firing_seq = state.firing_seq;
            let next = abandon(&state, failure_backoff_at_ms);
            persist_historian_state(store, session_id, next)?;
            Ok(RestartAction::AbandonedAndRefireEligible { firing_seq })
        }
    }
}

fn require_phase(
    current: &HistorianDurableState,
    expected: HistorianPhase,
    event: &'static str,
) -> Result<(), HistorianStateError> {
    if current.state == expected {
        Ok(())
    } else {
        Err(HistorianStateError::InvalidTransition {
            from: current.state.clone(),
            event,
        })
    }
}

fn idle_after_success(firing_seq: u64) -> HistorianDurableState {
    HistorianDurableState {
        firing_seq,
        ..HistorianDurableState::default()
    }
}

fn abandon_matching_run(
    store: &McStore,
    session_id: &str,
    predicate: &HistorianPublishPredicate,
    failure_backoff_at_ms: i64,
) -> Result<Option<u64>, HistorianStateError> {
    let loaded = store.load(session_id)?;
    let state = &loaded.meta.historian;
    let matches = state.firing_seq == predicate.firing_seq
        && state.producer_run_id.as_deref() == Some(predicate.producer_run_id.as_str())
        && state.chunk_fingerprint == predicate.chunk_fingerprint;
    if !matches || state.state == HistorianPhase::Idle {
        return Ok(None);
    }

    let mut meta = loaded.meta.clone();
    meta.historian = abandon(state, failure_backoff_at_ms);
    let row_version = store.commit(session_id, loaded.row_version, &loaded.core, &meta)?;
    Ok(Some(row_version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use mc_core::CoreState;
    use mc_store::{ModuleMeta, StoredCompartment};

    use crate::transform::{
        transform, CkItemWire, DeciderInputs, ProducerContext, TransformRequest,
    };

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

    fn item(id: &str, ordinal: u64, bytes: &str) -> CkItemWire {
        CkItemWire {
            id: id.to_string(),
            ordinal,
            bytes: bytes.to_string(),
            synthetic: false,
        }
    }

    fn req(items: Vec<CkItemWire>) -> TransformRequest {
        TransformRequest {
            session_id: "ses".to_string(),
            render_config: "cfg".to_string(),
            items,
        }
    }

    fn pctx<'a>() -> ProducerContext<'a> {
        ProducerContext {
            project_path: "git:proj",
            project_directory: "/nonexistent-docs",
            history_budget_tokens: 60_000.0,
            now_ms: 0,
        }
    }

    fn run_transform(store: &McStore, request: &TransformRequest) -> Vec<CkItemWire> {
        transform(store, request, &pctx(), &DeciderInputs::default())
            .unwrap()
            .ck_messages
    }

    fn comp(seq: i64, start: i64, end: i64, end_id: &str, p1: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: end_id.to_string(),
            title: format!("C{seq}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    fn publishing_state() -> HistorianDurableState {
        HistorianDurableState {
            state: HistorianPhase::Publishing,
            firing_seq: 3,
            chunk_range: Some(HistorianChunkRange {
                from_ordinal: 2,
                to_ordinal: 4,
            }),
            chunk_fingerprint: "fp".into(),
            producer_session_id: Some("producer-session".into()),
            producer_run_id: Some("run-3".into()),
            fired_at_ms: Some(10),
            failure_backoff_at_ms: None,
        }
    }

    #[test]
    fn chunk_fingerprint_uses_id_kind_and_byte_length() {
        let a = compute_chunk_fingerprint(&[
            ChunkSnapshotItem {
                id: "m1",
                kind: "user",
                bytes: "abc",
            },
            ChunkSnapshotItem {
                id: "m2",
                kind: "assistant",
                bytes: "å",
            },
        ]);
        let b = compute_chunk_fingerprint(&[
            ChunkSnapshotItem {
                id: "m1",
                kind: "user",
                bytes: "xyz",
            },
            ChunkSnapshotItem {
                id: "m2",
                kind: "assistant",
                bytes: "ø",
            },
        ]);
        assert_eq!(a, b, "same ids/kinds/byte lengths fingerprint the same");
        assert_eq!(a, "m1:user:3|m2:assistant:2");
    }

    #[test]
    fn pure_state_machine_happy_path_and_single_flight() {
        let idle = HistorianDurableState::default();
        let fired = match fire(&idle, 2, 5, "fp".into(), 100).unwrap() {
            FireOutcome::Fired(state) => state,
            FireOutcome::Busy(_) => panic!("idle state must fire"),
        };
        assert_eq!(fired.state, HistorianPhase::Firing);
        assert_eq!(fired.firing_seq, 1);
        assert!(matches!(
            fire(&fired, 6, 7, "other".into(), 101).unwrap(),
            FireOutcome::Busy(_)
        ));

        let awaiting = producer_started(&fired, "ps".into(), "run".into()).unwrap();
        let validating = output_received(&awaiting, "text").unwrap();
        let publishing = validation_ok(&validating).unwrap();
        let idle_again = tx_committed(&publishing).unwrap();
        assert_eq!(idle_again.state, HistorianPhase::Idle);
        assert_eq!(idle_again.firing_seq, 1);
    }

    #[test]
    fn fingerprint_mismatch_at_publish_abandons_and_releases_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let meta = ModuleMeta {
            historian: publishing_state(),
            ..Default::default()
        };
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        let loaded = store.load("ses").unwrap();
        let predicate = publish_predicate(&loaded.meta.historian).unwrap();
        let err = publish_validated_chunk(
            &store,
            ValidatedPublishRequest {
                session_id: "ses",
                project_path: "git:proj",
                expected_row_version: loaded.row_version,
                predicate: &predicate,
                observed_chunk_fingerprint: "different-fingerprint",
                validated: &ValidatedChunk::default(),
                publication_floor_ordinal: 5,
                failure_backoff_at_ms: 999,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            HistorianStateError::FingerprintMismatch { .. }
        ));

        let after = store.load("ses").unwrap().meta.historian;
        assert_eq!(after.state, HistorianPhase::Idle);
        assert_eq!(after.failure_backoff_at_ms, Some(999));
        assert!(matches!(
            fire(&after, 6, 7, "new".into(), 1000).unwrap(),
            FireOutcome::Fired(_)
        ));
    }

    #[test]
    fn restart_mid_awaiting_exposes_reattach_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let awaiting = producer_started(
            &match fire(&HistorianDurableState::default(), 1, 3, "fp".into(), 10).unwrap() {
                FireOutcome::Fired(state) => state,
                FireOutcome::Busy(_) => unreachable!(),
            },
            "producer-session".into(),
            "run-1".into(),
        )
        .unwrap();
        let meta = ModuleMeta {
            historian: awaiting,
            ..Default::default()
        };
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();

        let action = handle_restart_load(&store, "ses", 500).unwrap();
        assert_eq!(
            action,
            RestartAction::ReattachProducer {
                producer_session_id: "producer-session".into(),
                producer_run_id: "run-1".into(),
                firing_seq: 1,
                chunk_fingerprint: "fp".into(),
            }
        );
        assert_eq!(
            store.load("ses").unwrap().meta.historian.state,
            HistorianPhase::AwaitingProducer,
            "reattach does not clear the durable single-flight"
        );
    }

    #[test]
    fn restart_mid_publishing_with_committed_tx_detects_idle() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let meta = ModuleMeta {
            historian: publishing_state(),
            ..Default::default()
        };
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        let loaded = store.load("ses").unwrap();
        let predicate = publish_predicate(&loaded.meta.historian).unwrap();
        store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: loaded.row_version,
                predicate: &predicate,
                project_path: "git:proj",
                compartments: &[comp(1, 2, 4, "m4", "summary")],
                facts: &[],
                publication_floor_ordinal: 5,
            })
            .unwrap();

        assert_eq!(
            handle_restart_load(&store, "ses", 500).unwrap(),
            RestartAction::Done
        );
        assert_eq!(
            store.load("ses").unwrap().meta.historian.state,
            HistorianPhase::Idle
        );
    }

    #[test]
    fn publish_floor_only_between_defers_is_byte_invisible_to_transform() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .replace_compartments("ses", &[comp(1, 1, 1, "m1", "SUMMARY")])
            .unwrap();
        store
            .seed_memory(1, "git:proj", "ARCHITECTURE", "existing fact", 70)
            .unwrap();
        let request = req(vec![item("m1", 1, "raw"), item("t2", 2, "tail")]);
        run_transform(&store, &request);
        let before = run_transform(&store, &request);

        let mut state = publishing_state();
        state.chunk_range = Some(HistorianChunkRange {
            from_ordinal: 2,
            to_ordinal: 2,
        });
        state.chunk_fingerprint = "tail-fp".into();
        state.producer_run_id = Some("run-3".into());
        let mut meta = store.load("ses").unwrap().meta;
        meta.historian = state;
        let row_version = store
            .commit(
                "ses",
                store.load("ses").unwrap().row_version,
                &store.load("ses").unwrap().core,
                &meta,
            )
            .unwrap();
        let predicate = HistorianPublishPredicate {
            firing_seq: 3,
            producer_run_id: "run-3".into(),
            chunk_fingerprint: "tail-fp".into(),
        };
        store
            .publish_historian_chunk(HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: Some(row_version),
                predicate: &predicate,
                project_path: "git:proj",
                compartments: &[],
                facts: &[FactCandidate {
                    category: "ARCHITECTURE".into(),
                    content: "existing fact".into(),
                    ..Default::default()
                }],
                publication_floor_ordinal: 3,
            })
            .unwrap();

        let after = run_transform(&store, &request);
        assert_eq!(
            after, before,
            "publication floor and deduped facts never render"
        );
        let loaded = store.load("ses").unwrap();
        assert_eq!(loaded.meta.publication_floor_ordinal, Some(3));
        assert_eq!(loaded.meta.coverage_ordinal, Some(1));
    }
}
