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

use crate::historian_producer::{
    HistorianProducer, HistorianProducerError, ProducerOutput, RunHandle, RunState,
};
use crate::historian_validate::{
    validate_historian_output, HistorianChunk, HistorianValidationError, StoredCompartmentRange,
    ValidateOptions, ValidatedChunk, ValidatedCompartment,
};

/// Project a validated compartment onto the durable store row shape. Validation
/// resolves the message-id endpoints and tiers; publication only stamps the
/// creation time and marks the row as v2 (non-legacy).
fn to_stored_compartment(c: &ValidatedCompartment, created_at_ms: i64) -> StoredCompartment {
    StoredCompartment {
        sequence: c.sequence as i64,
        start_message: c.start_message as i64,
        end_message: c.end_message as i64,
        start_message_id: c.start_message_id.clone(),
        end_message_id: c.end_message_id.clone(),
        title: c.title.clone(),
        content: c.content.clone(),
        p1: c.p1.clone(),
        p2: c.p2.clone(),
        p3: c.p3.clone(),
        p4: c.p4.clone(),
        importance: c.importance.map(|i| i as i32).unwrap_or(50),
        episode_type: c.episode_type.clone(),
        legacy: 0,
        created_at: created_at_ms,
    }
}

/// Project a validated fact candidate onto the store's promotion input. The
/// historian promotes facts with no importance/expiry/source at publish time —
/// classification and decay are later, cache-neutral passes.
fn to_store_fact(f: &crate::historian_validate::FactCandidate) -> FactCandidate {
    FactCandidate {
        category: f.category.clone(),
        content: f.content.clone(),
        importance: None,
        expires_at: None,
        source_session_id: None,
    }
}

/// One flat item in the pinned chunk snapshot used to guard producer output.
/// The fingerprint intentionally records byte lengths rather than content bytes:
/// insertion/removal and type/id changes alter the fingerprint, while unrelated
/// metadata drift and same-length content edits do not stale a snapshot.
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
    /// Creation timestamp stamped on the appended compartment rows.
    pub created_at_ms: i64,
    pub failure_backoff_at_ms: i64,
}

/// Publish after re-checking the chunk fingerprint at the commit point. A mismatch
/// abandons the matching firing before returning the typed error, so a future fire
/// is not blocked by the stale producer.
///
/// The validation module owns the [`ValidatedChunk`] shape (message-id endpoints,
/// tiers, discard-last healing); this boundary projects it onto the durable store
/// rows and drives the CAS-gated publish transaction. Facts promote as additive
/// inserts, so a publish only surfaces on the next materializing pass via the
/// compartment/memory watermarks — it never mutates cached render state.
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

    let compartments: Vec<StoredCompartment> = request
        .validated
        .compartments
        .iter()
        .map(|c| to_stored_compartment(c, request.created_at_ms))
        .collect();
    let facts: Vec<FactCandidate> = request.validated.facts.iter().map(to_store_fact).collect();

    Ok(store.publish_historian_chunk(HistorianPublishRequest {
        session_id: request.session_id,
        expected_row_version: request.expected_row_version,
        predicate: request.predicate,
        project_path: request.project_path,
        compartments: &compartments,
        facts: &facts,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianRunSuccess {
    pub row_version: u64,
    pub producer_session_id: String,
    pub producer_run_id: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistorianDriveOutcome {
    Completed(HistorianRunSuccess),
    Busy(HistorianDurableState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistorianReattachOutcome {
    Done,
    Published(HistorianRunSuccess),
    RefireEligible { firing_seq: u64 },
}

#[derive(Debug)]
pub enum HistorianDriveError {
    NoModels,
    State(HistorianStateError),
    Producer(HistorianProducerError),
    Validation(HistorianValidationError),
}

impl fmt::Display for HistorianDriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistorianDriveError::NoModels => write!(f, "historian model chain is empty"),
            HistorianDriveError::State(e) => write!(f, "state: {e}"),
            HistorianDriveError::Producer(e) => write!(f, "producer: {e}"),
            HistorianDriveError::Validation(e) => write!(f, "validation: {e}"),
        }
    }
}

impl std::error::Error for HistorianDriveError {}

impl From<HistorianStateError> for HistorianDriveError {
    fn from(e: HistorianStateError) -> Self {
        HistorianDriveError::State(e)
    }
}

impl From<HistorianProducerError> for HistorianDriveError {
    fn from(e: HistorianProducerError) -> Self {
        HistorianDriveError::Producer(e)
    }
}

impl From<HistorianValidationError> for HistorianDriveError {
    fn from(e: HistorianValidationError) -> Self {
        HistorianDriveError::Validation(e)
    }
}

impl From<McStoreError> for HistorianDriveError {
    fn from(e: McStoreError) -> Self {
        HistorianDriveError::State(HistorianStateError::Store(e))
    }
}

#[subc_client_rs::async_trait(?Send)]
pub trait HistorianProducerDriver {
    async fn bind_session(&mut self, session_id: &str) -> Result<(), HistorianProducerError>;
    async fn start(
        &mut self,
        session_id: &str,
        system: &str,
        prompt: &str,
        model: &str,
    ) -> Result<RunHandle, HistorianProducerError>;
    async fn await_output(
        &mut self,
        run_id: &str,
    ) -> Result<ProducerOutput, HistorianProducerError>;
    async fn status(&mut self, run_id: &str) -> Result<RunState, HistorianProducerError>;
    async fn cancel(&mut self, run_id: &str) -> Result<(), HistorianProducerError>;
    async fn close(&mut self);
}

#[subc_client_rs::async_trait(?Send)]
impl HistorianProducerDriver for HistorianProducer {
    async fn bind_session(&mut self, session_id: &str) -> Result<(), HistorianProducerError> {
        HistorianProducer::bind_session(self, session_id.to_string());
        Ok(())
    }

    async fn start(
        &mut self,
        session_id: &str,
        system: &str,
        prompt: &str,
        model: &str,
    ) -> Result<RunHandle, HistorianProducerError> {
        HistorianProducer::start(self, session_id, system, prompt, model).await
    }

    async fn await_output(
        &mut self,
        run_id: &str,
    ) -> Result<ProducerOutput, HistorianProducerError> {
        HistorianProducer::await_output(self, run_id).await
    }

    async fn status(&mut self, run_id: &str) -> Result<RunState, HistorianProducerError> {
        HistorianProducer::status(self, run_id).await
    }

    async fn cancel(&mut self, run_id: &str) -> Result<(), HistorianProducerError> {
        HistorianProducer::cancel(self, run_id).await
    }

    async fn close(&mut self) {
        HistorianProducer::close(self).await;
    }
}

pub struct HistorianFireRequest<'a> {
    pub store: &'a McStore,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub project_slug: &'a str,
    /// The role-scoped historian SYSTEM prompt (HISTORIAN_SYSTEM_PROMPT). Sent via the
    /// producer's `system` field, never concatenated into `prompt`. Empty means absent.
    pub system: &'a str,
    pub prompt: &'a str,
    pub model_chain: &'a [String],
    pub from_ordinal: u64,
    pub to_ordinal: u64,
    pub chunk_fingerprint: &'a str,
    pub observed_chunk_fingerprint: &'a str,
    pub validation_chunk: &'a HistorianChunk,
    pub prior_compartments: &'a [StoredCompartmentRange],
    pub validate_options: ValidateOptions,
    pub now_ms: i64,
    pub failure_backoff_at_ms: i64,
}

pub struct HistorianReattachRequest<'a> {
    pub store: &'a McStore,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub observed_chunk_fingerprint: &'a str,
    pub validation_chunk: &'a HistorianChunk,
    pub prior_compartments: &'a [StoredCompartmentRange],
    pub validate_options: ValidateOptions,
    pub publication_floor_ordinal: u64,
    pub now_ms: i64,
    pub failure_backoff_at_ms: i64,
}

/// Build the llm-runner session id owned by Magic Context for one historian firing.
/// The firing sequence is part of the id so a fallback model attempt never resumes a
/// failed run under a different model.
pub fn historian_producer_session_id(project_slug: &str, firing_seq: u64) -> String {
    let slug: String = project_slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "project" } else { slug };
    format!("mc-historian:{slug}:{firing_seq}")
}

pub async fn run_historian_firing<P>(
    producer: &mut P,
    request: HistorianFireRequest<'_>,
) -> Result<HistorianDriveOutcome, HistorianDriveError>
where
    P: HistorianProducerDriver + ?Sized,
{
    if request.model_chain.is_empty() {
        return Err(HistorianDriveError::NoModels);
    }

    for (index, model) in request.model_chain.iter().enumerate() {
        verify_chunk_fingerprint(
            request.chunk_fingerprint,
            request.observed_chunk_fingerprint,
        )?;
        let loaded = request.store.load(request.session_id)?;
        let fired = match fire(
            &loaded.meta.historian,
            request.from_ordinal,
            request.to_ordinal,
            request.chunk_fingerprint.to_string(),
            request.now_ms,
        )? {
            FireOutcome::Busy(state) => return Ok(HistorianDriveOutcome::Busy(state)),
            FireOutcome::Fired(state) => state,
        };
        persist_historian_state(request.store, request.session_id, fired.clone())?;

        let producer_session_id =
            historian_producer_session_id(request.project_slug, fired.firing_seq);
        let handle = match producer
            .start(&producer_session_id, request.system, request.prompt, model)
            .await
        {
            Ok(handle) => handle,
            Err(err) => {
                let retry = err.is_retryable_model_failure()
                    && !err.is_abort_or_overflow()
                    && index + 1 < request.model_chain.len();
                persist_historian_state(
                    request.store,
                    request.session_id,
                    abandon(&fired, request.failure_backoff_at_ms),
                )?;
                producer.close().await;
                if retry {
                    continue;
                }
                return Err(HistorianDriveError::Producer(err));
            }
        };

        let awaiting =
            producer_started(&fired, producer_session_id.clone(), handle.run_id.clone())?;
        persist_historian_state(request.store, request.session_id, awaiting.clone())?;

        let output = match producer.await_output(&handle.run_id).await {
            Ok(output) => output,
            Err(err) => {
                let _ = producer.cancel(&handle.run_id).await;
                let retry = err.is_retryable_model_failure()
                    && !err.is_abort_or_overflow()
                    && index + 1 < request.model_chain.len();
                persist_historian_state(
                    request.store,
                    request.session_id,
                    abandon(&awaiting, request.failure_backoff_at_ms),
                )?;
                producer.close().await;
                if retry {
                    continue;
                }
                return Err(HistorianDriveError::Producer(err));
            }
        };

        // Always release both routes, whether publish succeeds or the validate/publish
        // path errors out — an early `?` return here would leak the command + subscribe
        // routes for this firing on the shared consumer connection.
        let publish_result = publish_output_from_awaiting(PublishOutputRequest {
            store: request.store,
            session_id: request.session_id,
            project_path: request.project_path,
            awaiting,
            output,
            observed_chunk_fingerprint: request.observed_chunk_fingerprint,
            validation_chunk: request.validation_chunk,
            prior_compartments: request.prior_compartments,
            validate_options: request.validate_options,
            created_at_ms: request.now_ms,
            failure_backoff_at_ms: request.failure_backoff_at_ms,
        });
        producer.close().await;
        let row_version = publish_result?;
        return Ok(HistorianDriveOutcome::Completed(HistorianRunSuccess {
            row_version,
            producer_session_id,
            producer_run_id: handle.run_id,
            model: model.clone(),
        }));
    }

    Err(HistorianDriveError::NoModels)
}

pub async fn reattach_historian_producer<P>(
    producer: &mut P,
    request: HistorianReattachRequest<'_>,
) -> Result<HistorianReattachOutcome, HistorianDriveError>
where
    P: HistorianProducerDriver + ?Sized,
{
    let action = handle_restart_load(
        request.store,
        request.session_id,
        request.failure_backoff_at_ms,
    )?;
    let RestartAction::ReattachProducer {
        producer_session_id,
        producer_run_id,
        firing_seq,
        ..
    } = action
    else {
        return Ok(match action {
            RestartAction::Done => HistorianReattachOutcome::Done,
            RestartAction::AbandonedAndRefireEligible { firing_seq } => {
                HistorianReattachOutcome::RefireEligible { firing_seq }
            }
            RestartAction::ReattachProducer { .. } => unreachable!(),
        });
    };

    producer.bind_session(&producer_session_id).await?;
    let state = match producer.status(&producer_run_id).await {
        Ok(state) => state,
        Err(_) => {
            abandon_current_state(
                request.store,
                request.session_id,
                request.failure_backoff_at_ms,
            )?;
            producer.close().await;
            return Ok(HistorianReattachOutcome::RefireEligible { firing_seq });
        }
    };

    match state {
        RunState::Terminal | RunState::Active => {}
        RunState::Missing { .. } => {
            abandon_current_state(
                request.store,
                request.session_id,
                request.failure_backoff_at_ms,
            )?;
            producer.close().await;
            return Ok(HistorianReattachOutcome::RefireEligible { firing_seq });
        }
    }

    let loaded = request.store.load(request.session_id)?;
    let awaiting = loaded.meta.historian.clone();
    let output = match producer.await_output(&producer_run_id).await {
        Ok(output) => output,
        Err(err) => {
            let _ = producer.cancel(&producer_run_id).await;
            abandon_current_state(
                request.store,
                request.session_id,
                request.failure_backoff_at_ms,
            )?;
            producer.close().await;
            return Err(HistorianDriveError::Producer(err));
        }
    };

    // Always release both routes, whether publish succeeds or errors — an early `?`
    // return would leak the command + subscribe routes for this reattached firing.
    let publish_result = publish_output_from_awaiting(PublishOutputRequest {
        store: request.store,
        session_id: request.session_id,
        project_path: request.project_path,
        awaiting,
        output,
        observed_chunk_fingerprint: request.observed_chunk_fingerprint,
        validation_chunk: request.validation_chunk,
        prior_compartments: request.prior_compartments,
        validate_options: request.validate_options,
        created_at_ms: request.now_ms,
        failure_backoff_at_ms: request.failure_backoff_at_ms,
    });
    producer.close().await;
    let row_version = publish_result?;
    Ok(HistorianReattachOutcome::Published(HistorianRunSuccess {
        row_version,
        producer_session_id,
        producer_run_id,
        model: String::new(),
    }))
}

struct PublishOutputRequest<'a> {
    store: &'a McStore,
    session_id: &'a str,
    project_path: &'a str,
    awaiting: HistorianDurableState,
    output: ProducerOutput,
    observed_chunk_fingerprint: &'a str,
    validation_chunk: &'a HistorianChunk,
    prior_compartments: &'a [StoredCompartmentRange],
    validate_options: ValidateOptions,
    created_at_ms: i64,
    failure_backoff_at_ms: i64,
}

fn publish_output_from_awaiting(
    request: PublishOutputRequest<'_>,
) -> Result<u64, HistorianDriveError> {
    let PublishOutputRequest {
        store,
        session_id,
        project_path,
        awaiting,
        output,
        observed_chunk_fingerprint,
        validation_chunk,
        prior_compartments,
        validate_options,
        created_at_ms,
        failure_backoff_at_ms,
    } = request;
    let validating = output_received(&awaiting, &output.text)?;
    persist_historian_state(store, session_id, validating.clone())?;

    let validated = match validate_historian_output(
        &output.text,
        validation_chunk,
        prior_compartments,
        validate_options,
    ) {
        Ok(validated) => validated,
        Err(err) => {
            persist_historian_state(
                store,
                session_id,
                abandon(&validating, failure_backoff_at_ms),
            )?;
            return Err(HistorianDriveError::Validation(err));
        }
    };

    let publishing = validation_ok(&validating)?;
    persist_historian_state(store, session_id, publishing.clone())?;
    let predicate = publish_predicate(&publishing)?;
    // The commit-point fingerprint re-check lives INSIDE publish_validated_chunk, which
    // abandons the matching firing (resetting the state to Idle+backoff) before returning
    // the mismatch error. A separate pre-check here would compare the same fingerprints
    // WITHOUT that abandon, so a mismatch would return early and strand the state in
    // Publishing forever — a wedged historian that never refires. Rely on the internal
    // guard as the single source of the check.
    let loaded = store.load(session_id)?;
    let published = publish_validated_chunk(
        store,
        ValidatedPublishRequest {
            session_id,
            project_path,
            expected_row_version: loaded.row_version,
            predicate: &predicate,
            observed_chunk_fingerprint,
            validated: &validated,
            publication_floor_ordinal: validated.unprocessed_from,
            created_at_ms,
            failure_backoff_at_ms,
        },
    )?;
    Ok(published.row_version)
}

fn abandon_current_state(
    store: &McStore,
    session_id: &str,
    failure_backoff_at_ms: i64,
) -> Result<(), HistorianStateError> {
    let loaded = store.load(session_id)?;
    persist_historian_state(
        store,
        session_id,
        abandon(&loaded.meta.historian, failure_backoff_at_ms),
    )?;
    Ok(())
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
    use std::collections::VecDeque;

    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use mc_core::CoreState;
    use mc_store::{ModuleMeta, StoredCompartment};

    use crate::transform::{
        ck_wire::{self, CkIngressMessage, CkWireMessage},
        transform, DeciderInputs, ProducerContext, TransformRequest,
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

    fn text_message(id: &str, text: &str) -> CkWireMessage {
        CkWireMessage::from_parts(
            "user",
            vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                text: text.to_string(),
            })],
            None,
            ck_wire::ProviderExtras::new(),
            ck_wire::HarnessMeta {
                harness_id: Some(id.to_string()),
                ..Default::default()
            },
        )
    }

    fn item(id: &str, ordinal: u64, bytes: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: id.to_string(),
            ordinal,
            ck: text_message(id, bytes),
        }
    }

    fn req(messages: Vec<CkIngressMessage>) -> TransformRequest {
        TransformRequest {
            kind: "transform".to_string(),
            v: 1,
            session_id: "ses".to_string(),
            render_config: "cfg".to_string(),
            messages,
            usage: None,
            agent_drop_ids: Vec::new(),
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

    fn run_transform(store: &McStore, request: &TransformRequest) -> Vec<CkWireMessage> {
        transform(store, request, &pctx(), &DeciderInputs::default())
            .unwrap()
            .ck_messages
    }

    fn comp(seq: i64, start: i64, end: i64, end_id: &str, p1: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: format!("{end_id}#0"),
            title: format!("C{seq}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    fn historian_xml(p1: &str) -> String {
        format!(
            r#"<output>
<compartments>
<compartment start="2" end="3" title="second arc" episode_type="feature" importance="60">
<p1>{p1}</p1>
<p2>second arc short</p2>
<p3>second arc</p3>
<p4 />
</compartment>
</compartments>
<meta><messages_processed>2-3</messages_processed><unprocessed_from>4</unprocessed_from></meta>
</output>"#
        )
    }

    fn historian_chunk() -> HistorianChunk {
        use crate::historian_validate::ChunkLine;
        HistorianChunk {
            start_index: 2,
            end_index: 4,
            lines: vec![
                ChunkLine {
                    ordinal: 2,
                    message_id: "m2#0".into(),
                },
                ChunkLine {
                    ordinal: 3,
                    message_id: "m3#0".into(),
                },
                ChunkLine {
                    ordinal: 4,
                    message_id: "m4#0".into(),
                },
            ],
            tool_only_ranges: vec![],
        }
    }

    fn prior_ranges() -> Vec<StoredCompartmentRange> {
        vec![StoredCompartmentRange {
            start_message: 1,
            end_message: 1,
        }]
    }

    fn validate_options() -> ValidateOptions {
        ValidateOptions {
            sequence_offset: 1,
            in_emergency: true,
        }
    }

    fn seed_prior_compartment(store: &McStore) {
        store
            .replace_compartments("ses", &[comp(1, 1, 1, "m1", "C1 summary")])
            .unwrap();
    }

    #[derive(Default)]
    struct ScriptedProducer {
        starts: VecDeque<Result<RunHandle, HistorianProducerError>>,
        outputs: VecDeque<Result<ProducerOutput, HistorianProducerError>>,
        statuses: VecDeque<Result<RunState, HistorianProducerError>>,
        observed_starts: Vec<(String, String)>,
        observed_sessions: Vec<String>,
        observed_systems: Vec<String>,
        await_run_ids: Vec<String>,
        cancels: Vec<String>,
        closes: usize,
    }

    impl ScriptedProducer {
        fn with_start(mut self, result: Result<RunHandle, HistorianProducerError>) -> Self {
            self.starts.push_back(result);
            self
        }

        fn with_output(mut self, result: Result<ProducerOutput, HistorianProducerError>) -> Self {
            self.outputs.push_back(result);
            self
        }

        fn with_status(mut self, result: Result<RunState, HistorianProducerError>) -> Self {
            self.statuses.push_back(result);
            self
        }
    }

    #[subc_client_rs::async_trait(?Send)]
    impl HistorianProducerDriver for ScriptedProducer {
        async fn bind_session(&mut self, session_id: &str) -> Result<(), HistorianProducerError> {
            self.observed_sessions.push(session_id.to_string());
            Ok(())
        }

        async fn start(
            &mut self,
            session_id: &str,
            system: &str,
            _prompt: &str,
            model: &str,
        ) -> Result<RunHandle, HistorianProducerError> {
            self.observed_sessions.push(session_id.to_string());
            self.observed_systems.push(system.to_string());
            self.observed_starts
                .push((session_id.to_string(), model.to_string()));
            self.starts
                .pop_front()
                .expect("scripted start result available")
        }

        async fn await_output(
            &mut self,
            run_id: &str,
        ) -> Result<ProducerOutput, HistorianProducerError> {
            self.await_run_ids.push(run_id.to_string());
            self.outputs
                .pop_front()
                .expect("scripted output result available")
        }

        async fn status(&mut self, _run_id: &str) -> Result<RunState, HistorianProducerError> {
            self.statuses
                .pop_front()
                .expect("scripted status result available")
        }

        async fn cancel(&mut self, run_id: &str) -> Result<(), HistorianProducerError> {
            self.cancels.push(run_id.to_string());
            Ok(())
        }

        async fn close(&mut self) {
            self.closes += 1;
        }
    }

    fn run_handle(id: &str) -> RunHandle {
        RunHandle {
            run_id: id.to_string(),
        }
    }

    fn producer_output(text: String) -> ProducerOutput {
        ProducerOutput { text }
    }

    fn fire_request<'a>(
        store: &'a McStore,
        prompt: &'a str,
        models: &'a [String],
        chunk: &'a HistorianChunk,
        prior: &'a [StoredCompartmentRange],
    ) -> HistorianFireRequest<'a> {
        HistorianFireRequest {
            store,
            session_id: "ses",
            project_path: "git:proj",
            project_slug: "proj",
            system: "role guidance",
            prompt,
            model_chain: models,
            from_ordinal: 2,
            to_ordinal: 4,
            chunk_fingerprint: "fp",
            observed_chunk_fingerprint: "fp",
            validation_chunk: chunk,
            prior_compartments: prior,
            validate_options: validate_options(),
            now_ms: 123,
            failure_backoff_at_ms: 999,
        }
    }

    fn reattach_request<'a>(
        store: &'a McStore,
        chunk: &'a HistorianChunk,
        prior: &'a [StoredCompartmentRange],
    ) -> HistorianReattachRequest<'a> {
        HistorianReattachRequest {
            store,
            session_id: "ses",
            project_path: "git:proj",
            observed_chunk_fingerprint: "fp",
            validation_chunk: chunk,
            prior_compartments: prior,
            validate_options: validate_options(),
            publication_floor_ordinal: 4,
            now_ms: 123,
            failure_backoff_at_ms: 999,
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

    #[tokio::test]
    async fn wired_historian_happy_path_sends_validates_and_publishes() {
        let dir = tempfile::tempdir().unwrap();
        let main_store = store(dir.path());
        seed_prior_compartment(&main_store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let models = vec!["model-a".to_string()];
        let text = historian_xml("second arc full and exact");
        let mut producer = ScriptedProducer::default()
            .with_start(Ok(run_handle("run-1")))
            .with_output(Ok(producer_output(text)));

        let outcome = run_historian_firing(
            &mut producer,
            fire_request(&main_store, "placeholder prompt", &models, &chunk, &prior),
        )
        .await
        .unwrap();

        let HistorianDriveOutcome::Completed(success) = outcome else {
            panic!("expected completed outcome");
        };
        assert_eq!(success.model, "model-a");
        assert_eq!(success.producer_session_id, "mc-historian:proj:1");
        assert_eq!(producer.observed_starts.len(), 1);
        assert_eq!(producer.observed_starts[0].0, "mc-historian:proj:1");
        assert_eq!(
            producer.observed_systems,
            vec!["role guidance".to_string()],
            "exactly ONE send carries the system prompt; a reattach path never re-sends \
             (system rides the run's durable input, re-drained not re-sent)"
        );
        assert_eq!(producer.await_run_ids, vec!["run-1"]);

        let loaded = main_store.load("ses").unwrap();
        assert_eq!(loaded.meta.historian.state, HistorianPhase::Idle);
        assert_eq!(loaded.meta.publication_floor_ordinal, Some(4));
        let comps = main_store.load_compartments("ses").unwrap();
        assert_eq!(
            comps.len(),
            2,
            "prior C1 preserved and historian C2 appended"
        );
        let c2 = comps.last().unwrap();
        assert_eq!(c2.end_message_id, "m3#0");
        assert_eq!(c2.p1.as_deref(), Some("second arc full and exact"));
        assert_eq!(c2.created_at, 123);
    }

    #[tokio::test]
    async fn fallback_retry_uses_new_session_and_overflow_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let fallback_store = store(dir.path());
        seed_prior_compartment(&fallback_store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let mut producer = ScriptedProducer::default()
            .with_start(Err(HistorianProducerError::retryable_model_failure(
                "provider overloaded",
            )))
            .with_start(Ok(run_handle("run-2")))
            .with_output(Ok(producer_output(historian_xml("fallback model summary"))));

        let outcome = run_historian_firing(
            &mut producer,
            fire_request(
                &fallback_store,
                "placeholder prompt",
                &models,
                &chunk,
                &prior,
            ),
        )
        .await
        .unwrap();
        let HistorianDriveOutcome::Completed(success) = outcome else {
            panic!("expected completed fallback outcome");
        };
        assert_eq!(success.model, "model-b");
        assert_eq!(
            producer.observed_starts,
            vec![
                ("mc-historian:proj:1".to_string(), "model-a".to_string()),
                ("mc-historian:proj:2".to_string(), "model-b".to_string()),
            ],
            "fallback retries author a new session/run instead of resuming under another model"
        );
        assert_eq!(
            fallback_store
                .load("ses")
                .unwrap()
                .meta
                .historian
                .firing_seq,
            2
        );

        let dir = tempfile::tempdir().unwrap();
        let overflow_store = store(dir.path());
        seed_prior_compartment(&overflow_store);
        let mut overflow = ScriptedProducer::default().with_start(Err(
            HistorianProducerError::context_overflow("context window exceeded"),
        ));
        let err = run_historian_firing(
            &mut overflow,
            fire_request(
                &overflow_store,
                "placeholder prompt",
                &models,
                &chunk,
                &prior,
            ),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HistorianDriveError::Producer(_)));
        assert_eq!(
            overflow.observed_starts.len(),
            1,
            "overflow does not try the next model"
        );
        let state = overflow_store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert_eq!(state.firing_seq, 1);
    }

    #[tokio::test]
    async fn reattach_terminal_redrains_from_start_without_second_send() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let fired = match fire(&HistorianDurableState::default(), 2, 4, "fp".into(), 1).unwrap() {
            FireOutcome::Fired(state) => state,
            FireOutcome::Busy(_) => unreachable!(),
        };
        let awaiting = producer_started(&fired, "producer-session".into(), "run-1".into()).unwrap();
        store
            .commit(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta {
                    historian: awaiting,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut producer = ScriptedProducer::default()
            .with_status(Ok(RunState::Terminal))
            .with_output(Ok(producer_output(historian_xml(
                "terminal replay summary",
            ))));

        let outcome =
            reattach_historian_producer(&mut producer, reattach_request(&store, &chunk, &prior))
                .await
                .unwrap();
        assert!(matches!(outcome, HistorianReattachOutcome::Published(_)));
        assert!(
            producer.observed_starts.is_empty(),
            "reattach publishes replayed output without a second session.send"
        );
        assert_eq!(producer.observed_sessions, vec!["producer-session"]);
        assert_eq!(producer.await_run_ids, vec!["run-1"]);
        let c2 = store.load_compartments("ses").unwrap().pop().unwrap();
        assert_eq!(c2.p1.as_deref(), Some("terminal replay summary"));
    }

    #[tokio::test]
    async fn reattach_redrains_full_run_from_start() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let fired = match fire(&HistorianDurableState::default(), 2, 4, "fp".into(), 1).unwrap() {
            FireOutcome::Fired(state) => state,
            FireOutcome::Busy(_) => unreachable!(),
        };
        let awaiting = producer_started(&fired, "producer-session".into(), "run-1".into()).unwrap();
        store
            .commit(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta {
                    historian: awaiting,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut producer = ScriptedProducer::default()
            .with_status(Ok(RunState::Terminal))
            .with_output(Ok(producer_output(historian_xml("full replay summary"))));

        let outcome =
            reattach_historian_producer(&mut producer, reattach_request(&store, &chunk, &prior))
                .await
                .unwrap();

        assert!(matches!(outcome, HistorianReattachOutcome::Published(_)));
        assert!(
            producer.observed_starts.is_empty(),
            "re-draining from the start is a subscribe-only reattach, not a new send"
        );
        assert_eq!(producer.await_run_ids, vec!["run-1"]);
        let c2 = store.load_compartments("ses").unwrap().pop().unwrap();
        assert_eq!(c2.p1.as_deref(), Some("full replay summary"));
    }

    #[tokio::test]
    async fn reattach_missing_abandons_and_releases_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let fired = match fire(&HistorianDurableState::default(), 2, 4, "fp".into(), 1).unwrap() {
            FireOutcome::Fired(state) => state,
            FireOutcome::Busy(_) => unreachable!(),
        };
        let awaiting = producer_started(&fired, "producer-session".into(), "run-1".into()).unwrap();
        store
            .commit(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta {
                    historian: awaiting,
                    ..Default::default()
                },
            )
            .unwrap();
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let mut producer = ScriptedProducer::default().with_status(Ok(RunState::Missing {
            detail: Some("gone".into()),
        }));

        let outcome =
            reattach_historian_producer(&mut producer, reattach_request(&store, &chunk, &prior))
                .await
                .unwrap();
        assert_eq!(
            outcome,
            HistorianReattachOutcome::RefireEligible { firing_seq: 1 }
        );
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert!(matches!(
            fire(&state, 2, 4, "fp2".into(), 2).unwrap(),
            FireOutcome::Fired(_)
        ));
    }

    #[tokio::test]
    async fn producer_timeout_abandons_and_best_effort_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let models = vec!["model-a".to_string()];
        let mut producer = ScriptedProducer::default()
            .with_start(Ok(run_handle("run-1")))
            .with_output(Err(HistorianProducerError::TimedOut));

        let err = run_historian_firing(
            &mut producer,
            fire_request(&store, "placeholder prompt", &models, &chunk, &prior),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HistorianDriveError::Producer(_)));
        assert_eq!(producer.cancels, vec!["run-1"]);
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert_eq!(state.failure_backoff_at_ms, Some(999));
    }

    #[tokio::test]
    async fn run_paused_abandons_and_refires() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let models = vec!["model-a".to_string(), "model-b".to_string()];
        let mut producer = ScriptedProducer::default()
            .with_start(Ok(run_handle("run-1")))
            .with_output(Err(HistorianProducerError::RunPaused {
                run_id: "run-1".into(),
                reason: Some("auth_required".into()),
            }));

        let err = run_historian_firing(
            &mut producer,
            fire_request(&store, "placeholder prompt", &models, &chunk, &prior),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, HistorianDriveError::Producer(_)));
        assert_eq!(
            producer.observed_starts,
            vec![("mc-historian:proj:1".to_string(), "model-a".to_string())],
            "paused runs abandon the slot instead of retrying the next model"
        );
        assert_eq!(producer.cancels, vec!["run-1"]);
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert_eq!(state.failure_backoff_at_ms, Some(999));
        assert!(matches!(
            fire(&state, 2, 4, "fp2".into(), 124).unwrap(),
            FireOutcome::Fired(_)
        ));
    }

    #[tokio::test]
    async fn reattach_fingerprint_mismatch_recovers_to_idle_and_releases_routes() {
        // A tail that changed under the historian makes the observed fingerprint differ
        // from the frozen one at publish time. The mismatch must abandon the firing back
        // to Idle (single source of the check lives inside publish_validated_chunk) — the
        // historian must NOT wedge in Publishing — and both routes must be released.
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let fired = match fire(&HistorianDurableState::default(), 2, 4, "fp".into(), 1).unwrap() {
            FireOutcome::Fired(state) => state,
            FireOutcome::Busy(_) => unreachable!(),
        };
        let awaiting = producer_started(&fired, "producer-session".into(), "run-1".into()).unwrap();
        store
            .commit(
                "ses",
                None,
                &CoreState::default(),
                &ModuleMeta {
                    historian: awaiting,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut producer = ScriptedProducer::default()
            .with_status(Ok(RunState::Terminal))
            .with_output(Ok(producer_output(historian_xml(
                "summary for a changed tail",
            ))));

        // observed fingerprint diverges from the stored "fp" — a tail change since firing.
        let request = HistorianReattachRequest {
            store: &store,
            session_id: "ses",
            project_path: "git:proj",
            observed_chunk_fingerprint: "fp-changed",
            validation_chunk: &chunk,
            prior_compartments: &prior,
            validate_options: validate_options(),
            publication_floor_ordinal: 4,
            now_ms: 123,
            failure_backoff_at_ms: 999,
        };
        let err = reattach_historian_producer(&mut producer, request)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            HistorianDriveError::State(HistorianStateError::FingerprintMismatch { .. })
        ));
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(
            state.state,
            HistorianPhase::Idle,
            "fingerprint mismatch must recover to Idle, never wedge in Publishing"
        );
        assert_eq!(state.failure_backoff_at_ms, Some(999));
        assert!(
            producer.closes >= 1,
            "routes must be released on the error path"
        );
    }

    #[tokio::test]
    async fn fresh_path_validation_rejection_releases_routes() {
        // A publish/validate failure on the fresh firing path must still release the
        // producer routes — an early `?` return before close would leak them.
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        seed_prior_compartment(&store);
        let chunk = historian_chunk();
        let prior = prior_ranges();
        let models = vec!["model-a".to_string()];
        let mut producer = ScriptedProducer::default()
            .with_start(Ok(run_handle("run-1")))
            .with_output(Ok(producer_output(
                "not a valid historian document".to_string(),
            )));

        let err = run_historian_firing(
            &mut producer,
            fire_request(&store, "placeholder prompt", &models, &chunk, &prior),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HistorianDriveError::Validation(_)));
        assert_eq!(
            producer.closes, 1,
            "the route must be closed even when validate/publish errors out"
        );
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
    }

    /// The seam-close proof: a real historian output is parsed + validated by the
    /// validation module, and the resulting `ValidatedChunk` drives the publish
    /// path end to end. This is the capstone that both parallel units are correct
    /// TOGETHER — the validator's message-id endpoints and tiers land as durable
    /// compartment rows, and the publish stays defer-invisible.
    #[test]
    fn validated_output_drives_publish_end_to_end() {
        use crate::historian_validate::{
            validate_historian_output, ChunkLine, HistorianChunk, ValidateOptions,
        };

        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // m0 already folds C1 (covers ordinal 1); ordinals 2..=4 are the chunk the
        // historian just summarized into C2.
        store
            .replace_compartments("ses", &[comp(1, 1, 1, "m1", "C1 summary")])
            .unwrap();

        let text = r#"<output>
<compartments>
<compartment start="2" end="3" title="second arc" episode_type="feature" importance="60">
<p1>second arc full and exact</p1>
<p2>second arc short</p2>
<p3>second arc</p3>
<p4 />
</compartment>
</compartments>
<meta><messages_processed>2-3</messages_processed><unprocessed_from>4</unprocessed_from></meta>
</output>"#;
        let chunk = HistorianChunk {
            start_index: 2,
            end_index: 4,
            lines: vec![
                ChunkLine {
                    ordinal: 2,
                    message_id: "m2#0".into(),
                },
                ChunkLine {
                    ordinal: 3,
                    message_id: "m3#0".into(),
                },
                ChunkLine {
                    ordinal: 4,
                    message_id: "m4#0".into(),
                },
            ],
            tool_only_ranges: vec![],
        };
        let prior = [crate::historian_validate::StoredCompartmentRange {
            start_message: 1,
            end_message: 1,
        }];
        let validated = validate_historian_output(
            text,
            &chunk,
            &prior,
            ValidateOptions {
                sequence_offset: 1,
                in_emergency: true, // skip discard-last so the single compartment persists
            },
        )
        .expect("validation succeeds");
        assert_eq!(validated.compartments.len(), 1);
        assert_eq!(validated.compartments[0].end_message_id, "m3#0");

        // Drive the state machine to a publishing row and publish the validated chunk.
        let mut meta = store.load("ses").unwrap().meta;
        meta.historian = HistorianDurableState {
            state: HistorianPhase::Publishing,
            firing_seq: 1,
            chunk_range: Some(HistorianChunkRange {
                from_ordinal: 2,
                to_ordinal: 4,
            }),
            chunk_fingerprint: "fp".into(),
            producer_session_id: Some("ps".into()),
            producer_run_id: Some("run-1".into()),
            fired_at_ms: Some(1),
            failure_backoff_at_ms: None,
        };
        let rv = store
            .commit(
                "ses",
                store.load("ses").unwrap().row_version,
                &store.load("ses").unwrap().core,
                &meta,
            )
            .unwrap();
        let predicate = publish_predicate(&meta.historian).unwrap();

        publish_validated_chunk(
            &store,
            ValidatedPublishRequest {
                session_id: "ses",
                project_path: "git:proj",
                expected_row_version: Some(rv),
                predicate: &predicate,
                observed_chunk_fingerprint: "fp",
                validated: &validated,
                publication_floor_ordinal: 4,
                created_at_ms: 123,
                failure_backoff_at_ms: 0,
            },
        )
        .expect("publish succeeds");

        // The validated compartment landed as a durable v2 row with the resolved
        // end message id and tier, and the state machine returned to idle.
        let after = store.load("ses").unwrap();
        assert_eq!(after.meta.historian.state, HistorianPhase::Idle);
        assert_eq!(after.meta.publication_floor_ordinal, Some(4));
        let comps = store.load_compartments("ses").unwrap();
        assert_eq!(comps.len(), 2, "C1 preserved, C2 appended");
        let c2 = comps.last().unwrap();
        assert_eq!(c2.end_message_id, "m3#0");
        assert_eq!(c2.p1.as_deref(), Some("second arc full and exact"));
        assert_eq!(c2.legacy, 0);
        assert_eq!(c2.created_at, 123);
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
                created_at_ms: 0,
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
