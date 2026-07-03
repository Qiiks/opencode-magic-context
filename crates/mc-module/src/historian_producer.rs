//! llm-runner session client used by the historian writer.
//!
//! The client intentionally speaks only the JSON session wire over subc routes. It
//! does not depend on llm-runner Rust crates, so Magic Context remains an origin-
//! agnostic consumer module.

use std::{error::Error, fmt, path::PathBuf, time::Duration};

use serde_json::{json, Value};
use subc_control::{ClientControlRequest, ClientControlResponse, ConsumerIdentity};
use subc_protocol::{
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, Priority, RouteTarget,
    SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::net::TcpStream;

const DEFAULT_LLM_RUNNER_MODULE_ID: &str = "llm-runner";

/// Output budget for a historian summarization pass. llm-runner's default (4k) truncated
/// a real 50k-input chunk mid-XML on the rig: a tiered compartment doc for a full chunk
/// legitimately needs five figures. The provider clamps to its own per-model limit, so a
/// generous request costs nothing unless the model actually generates that much.
const HISTORIAN_MAX_OUTPUT_TOKENS: u32 = 32_000;

/// Sampling temperature for historian runs. The prompt and this value were calibrated
/// TOGETHER: at provider-default temperature (1.0) flash-class models drift past the
/// prompt's exclusion rules and copy the format template and rotating-seed reference
/// compartments into their output (observed live on the rig with the calibration model
/// itself), while at 0.1 the same prompt extracts cleanly. Sending the prompt without
/// the temperature is running half the calibration.
const HISTORIAN_TEMPERATURE: f64 = 0.1;
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for a summarization run to finish. A historian pass legitimately
/// generates 10k+ output tokens and can run several minutes on a flash-class model; a
/// 120s window abandoned a run on the rig WHILE it was still successfully finishing
/// (the terminal arrived moments after the waiter gave up). A fold is a background
/// operation, never latency-sensitive: waiting longer and publishing always beats
/// abandoning a completed run and re-firing the whole 50k-input pass.
const DEFAULT_AWAIT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHandle {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerOutput {
    pub text: String,
    /// True when any unit in the run reported a length-class finish reason. The run
    /// terminal can still say completed while a model step hit its output ceiling and
    /// cut the text mid-document, so validation failures need this to self-diagnose.
    pub length_capped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Terminal,
    Active,
    Missing { detail: Option<String> },
}

#[derive(Debug, Clone)]
pub struct HistorianProducerConfig {
    pub connection_file: PathBuf,
    pub project_root: PathBuf,
    pub harness: String,
    pub module_id: String,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub await_timeout: Duration,
}

impl HistorianProducerConfig {
    pub fn new(
        connection_file: impl Into<PathBuf>,
        project_root: impl Into<PathBuf>,
        harness: impl Into<String>,
    ) -> Self {
        Self {
            connection_file: connection_file.into(),
            project_root: project_root.into(),
            harness: harness.into(),
            module_id: DEFAULT_LLM_RUNNER_MODULE_ID.to_string(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            await_timeout: DEFAULT_AWAIT_TIMEOUT,
        }
    }
}

#[derive(Debug)]
pub enum HistorianProducerError {
    ConnectionFile {
        path: PathBuf,
        source: ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    Connect {
        endpoint: String,
        source: std::io::Error,
    },
    Auth(AuthError),
    FrameIo(FrameIoError),
    FrameBuild(FrameBuildError),
    Json(serde_json::Error),
    Subc(ErrorBody),
    UnexpectedControlResponse,
    MissingRunId,
    MissingSession,
    UnexpectedStreamEnd,
    TimedOut,
    RunFailed {
        run_id: String,
        detail: String,
    },
    TerminalRunMismatch {
        expected: String,
        found: Option<String>,
    },
    RunPaused {
        run_id: String,
        reason: Option<String>,
    },
}

impl HistorianProducerError {
    pub fn retryable_model_failure(message: impl Into<String>) -> Self {
        HistorianProducerError::Subc(ErrorBody {
            code: "retryable_model_failure".to_string(),
            message: message.into(),
        })
    }

    pub fn context_overflow(message: impl Into<String>) -> Self {
        HistorianProducerError::Subc(ErrorBody {
            code: "context_overflow".to_string(),
            message: message.into(),
        })
    }

    pub fn aborted(message: impl Into<String>) -> Self {
        HistorianProducerError::Subc(ErrorBody {
            code: "aborted".to_string(),
            message: message.into(),
        })
    }

    pub fn is_retryable_model_failure(&self) -> bool {
        match self {
            HistorianProducerError::Subc(body) => {
                retryable_code(&body.code) || retryable_code(&body.message)
            }
            HistorianProducerError::RunFailed { detail, .. } => retryable_code(detail),
            _ => false,
        }
    }

    pub fn is_abort_or_overflow(&self) -> bool {
        match self {
            HistorianProducerError::Subc(body) => {
                abort_or_overflow(&body.code) || abort_or_overflow(&body.message)
            }
            HistorianProducerError::RunFailed { detail, .. } => abort_or_overflow(detail),
            _ => false,
        }
    }
}

fn retryable_code(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("retry")
        || s.contains("transient")
        || s.contains("rate_limit")
        || s.contains("provider_unavailable")
        || s.contains("overloaded")
}

fn abort_or_overflow(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.contains("abort")
        || s.contains("cancel")
        || s.contains("context_overflow")
        || s.contains("overflow")
}

impl fmt::Display for HistorianProducerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistorianProducerError::ConnectionFile { path, source } => {
                write!(f, "read connection file {}: {source}", path.display())
            }
            HistorianProducerError::NoEndpoint { path } => {
                write!(f, "connection file {} has no endpoint", path.display())
            }
            HistorianProducerError::Connect { endpoint, source } => {
                write!(f, "connect to {endpoint}: {source}")
            }
            HistorianProducerError::Auth(e) => write!(f, "authenticate to subc: {e}"),
            HistorianProducerError::FrameIo(e) => write!(f, "subc frame I/O: {e}"),
            HistorianProducerError::FrameBuild(e) => write!(f, "build subc frame: {e}"),
            HistorianProducerError::Json(e) => write!(f, "json: {e}"),
            HistorianProducerError::Subc(body) => {
                write!(f, "subc error {}: {}", body.code, body.message)
            }
            HistorianProducerError::UnexpectedControlResponse => {
                write!(f, "route.open returned an unexpected control response")
            }
            HistorianProducerError::MissingRunId => {
                write!(f, "session.send did not return an active run_id")
            }
            HistorianProducerError::MissingSession => {
                write!(f, "historian producer has no bound session")
            }
            HistorianProducerError::UnexpectedStreamEnd => write!(
                f,
                "subscribe stream ended before the run terminal control unit"
            ),
            HistorianProducerError::TimedOut => write!(f, "historian producer timed out"),
            HistorianProducerError::RunFailed { run_id, detail } => {
                write!(f, "run {run_id} failed: {detail}")
            }
            HistorianProducerError::TerminalRunMismatch { expected, found } => {
                write!(
                    f,
                    "run {expected} received terminal control unit after RunStarted {:?}",
                    found
                )
            }
            HistorianProducerError::RunPaused { run_id, reason } => {
                write!(f, "run {run_id} paused")?;
                if let Some(reason) = reason {
                    write!(f, ": {reason}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for HistorianProducerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            HistorianProducerError::ConnectionFile { source, .. } => Some(source),
            HistorianProducerError::Connect { source, .. } => Some(source),
            HistorianProducerError::Auth(e) => Some(e),
            HistorianProducerError::FrameIo(e) => Some(e),
            HistorianProducerError::FrameBuild(e) => Some(e),
            HistorianProducerError::Json(e) => Some(e),
            HistorianProducerError::NoEndpoint { .. }
            | HistorianProducerError::Subc(_)
            | HistorianProducerError::UnexpectedControlResponse
            | HistorianProducerError::MissingRunId
            | HistorianProducerError::MissingSession
            | HistorianProducerError::UnexpectedStreamEnd
            | HistorianProducerError::TimedOut
            | HistorianProducerError::RunFailed { .. }
            | HistorianProducerError::TerminalRunMismatch { .. }
            | HistorianProducerError::RunPaused { .. } => None,
        }
    }
}

impl From<FrameIoError> for HistorianProducerError {
    fn from(e: FrameIoError) -> Self {
        HistorianProducerError::FrameIo(e)
    }
}

impl From<FrameBuildError> for HistorianProducerError {
    fn from(e: FrameBuildError) -> Self {
        HistorianProducerError::FrameBuild(e)
    }
}

impl From<serde_json::Error> for HistorianProducerError {
    fn from(e: serde_json::Error) -> Self {
        HistorianProducerError::Json(e)
    }
}

pub struct HistorianProducer {
    config: HistorianProducerConfig,
    stream: TcpStream,
    next_corr: u64,
    session_id: Option<String>,
    command_route: Option<u16>,
    subscribe_route: Option<u16>,
}

impl HistorianProducer {
    pub async fn connect(config: HistorianProducerConfig) -> Result<Self, HistorianProducerError> {
        let conn = connection_file::read(&config.connection_file).map_err(|source| {
            HistorianProducerError::ConnectionFile {
                path: config.connection_file.clone(),
                source,
            }
        })?;
        let endpoint =
            conn.endpoints
                .first()
                .ok_or_else(|| HistorianProducerError::NoEndpoint {
                    path: config.connection_file.clone(),
                })?;
        let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
        let mut stream = TcpStream::connect(&endpoint_label)
            .await
            .map_err(|source| HistorianProducerError::Connect {
                endpoint: endpoint_label,
                source,
            })?;
        authenticate_client(&mut stream, &conn, config.handshake_timeout)
            .await
            .map_err(HistorianProducerError::Auth)?;
        Ok(Self {
            config,
            stream,
            next_corr: 1,
            session_id: None,
            command_route: None,
            subscribe_route: None,
        })
    }

    /// Bind subsequent status/subscribe/cancel calls to an existing session. Reattach
    /// probes open the route with the persisted session id and must not call send again.
    pub fn bind_session(&mut self, session_id: impl Into<String>) {
        self.session_id = Some(session_id.into());
    }

    pub async fn start(
        &mut self,
        session_id: &str,
        system: &str,
        prompt: &str,
        model: &str,
    ) -> Result<RunHandle, HistorianProducerError> {
        self.bind_session(session_id.to_string());
        let route = self.ensure_command_route().await?;
        // The route identity is the session. Keeping `session` out of this body avoids
        // a second, diverging source of truth for the run lineage.
        //
        // `system` rides the role-scoped SendParams.system field (delivered as a leading
        // system message in the run's durable input, byte-exact) — NEVER concatenated
        // into the user prompt: the historian's parse/validate contract assumes the
        // model saw its role guidance as a system message. Empty means absent, matching
        // the wire's empty-as-absent rule, so we omit the field entirely.
        //
        // The params shape mirrors llm-runner's SendParams (llmr-module-serve wire.rs):
        // `model` is a nested {provider, model} object, split from our canonical
        // "provider/model" string at the FIRST slash so multi-slash model names keep
        // their remainder intact. The server decodes strictly enough that a flat model
        // string fails the whole send with invalid_params, which a live rig drive
        // surfaced as firings dying before any producer run existed.
        let (provider, model_name) = model.split_once('/').ok_or_else(|| {
            HistorianProducerError::Subc(ErrorBody {
                code: "invalid_model".to_string(),
                message: format!("model '{model}' is not in canonical provider/model form"),
            })
        })?;
        let mut params = serde_json::Map::new();
        params.insert("prompt".into(), json!(prompt));
        params.insert(
            "model".into(),
            json!({ "provider": provider, "model": model_name }),
        );
        params.insert("tools".into(), json!([]));
        params.insert(
            "generation".into(),
            json!({
                "max_output_tokens": HISTORIAN_MAX_OUTPUT_TOKENS,
                "temperature": HISTORIAN_TEMPERATURE,
            }),
        );
        if !system.is_empty() {
            params.insert("system".into(), json!(system));
        }
        let body = json!({
            "method": "session.send",
            "params": params
        });
        let response = self.unary_json(route, body).await?;
        let run_id = response
            .get("run_id")
            .and_then(Value::as_str)
            .or_else(|| {
                response
                    .get("result")
                    .and_then(|r| r.get("run_id"))
                    .and_then(Value::as_str)
            })
            .ok_or(HistorianProducerError::MissingRunId)?;
        Ok(RunHandle {
            run_id: run_id.to_string(),
        })
    }

    pub async fn await_output(
        &mut self,
        run_id: &str,
    ) -> Result<ProducerOutput, HistorianProducerError> {
        let route = self.ensure_subscribe_route().await?;
        // Subscribe from "start" instead of a cursor. Replay after a cursor is
        // exclusive, so persisting an advancing cursor can drop units at or before
        // it on reattach. Re-draining from the start is safe because validation
        // and compare-and-swap checks during publish are idempotent.
        let body = json!({ "method": "session.subscribe", "params": { "from": "start" } });
        let corr = self.send_request(route, body).await?;
        match tokio::time::timeout(
            self.config.await_timeout,
            self.drain_subscribe(route, corr, run_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(HistorianProducerError::TimedOut),
        }
    }

    pub async fn status(&mut self, run_id: &str) -> Result<RunState, HistorianProducerError> {
        let route = self.ensure_command_route().await?;
        let response = self
            .unary_json(
                route,
                json!({ "method": "run.status", "params": { "run_id": run_id } }),
            )
            .await?;
        Ok(classify_run_state(run_id, &response))
    }

    pub async fn cancel(&mut self, run_id: &str) -> Result<(), HistorianProducerError> {
        let route = self.ensure_command_route().await?;
        let _ = self
            .unary_json(
                route,
                json!({ "method": "run.cancel", "params": { "run_id": run_id } }),
            )
            .await?;
        Ok(())
    }

    pub async fn close(&mut self) {
        if let Some(route) = self.subscribe_route.take() {
            let _ = self.send_goodbye(route).await;
        }
        if let Some(route) = self.command_route.take() {
            let _ = self.send_goodbye(route).await;
        }
    }

    async fn ensure_command_route(&mut self) -> Result<u16, HistorianProducerError> {
        if let Some(route) = self.command_route {
            return Ok(route);
        }
        let route = self.open_bound_route().await?;
        self.command_route = Some(route);
        Ok(route)
    }

    async fn ensure_subscribe_route(&mut self) -> Result<u16, HistorianProducerError> {
        if let Some(route) = self.subscribe_route {
            return Ok(route);
        }
        let route = self.open_bound_route().await?;
        self.subscribe_route = Some(route);
        Ok(route)
    }

    async fn open_bound_route(&mut self) -> Result<u16, HistorianProducerError> {
        let session = self
            .session_id
            .clone()
            .ok_or(HistorianProducerError::MissingSession)?;
        let request = ClientControlRequest::RouteOpen {
            target: RouteTarget::ManagementSurface {
                module_id: self.config.module_id.clone(),
            },
            identity: BindIdentity {
                project_root: self.config.project_root.clone(),
                harness: self.config.harness.clone(),
                session,
            },
            consumer_identity: consumer_identity_from_env(),
        };
        let corr = self.next_corr();
        let body = serde_json::to_vec(&request)?;
        self.write_frame(FrameType::Request, 0, corr, body).await?;
        let frame = self
            .read_terminal_for(0, corr, self.config.request_timeout)
            .await?;
        match frame.header.ty {
            FrameType::Response => {
                let response: ClientControlResponse = serde_json::from_slice(&frame.body)?;
                if let ClientControlResponse::RouteOpen { route_channel } = response {
                    Ok(route_channel)
                } else {
                    Err(HistorianProducerError::UnexpectedControlResponse)
                }
            }
            FrameType::Error => Err(HistorianProducerError::Subc(error_body(&frame.body))),
            _ => Err(HistorianProducerError::UnexpectedControlResponse),
        }
    }

    async fn unary_json(
        &mut self,
        channel: u16,
        body: Value,
    ) -> Result<Value, HistorianProducerError> {
        let corr = self.send_request(channel, body).await?;
        let frame = self
            .read_terminal_for(channel, corr, self.config.request_timeout)
            .await?;
        match frame.header.ty {
            FrameType::Response => Ok(serde_json::from_slice(&frame.body)?),
            FrameType::StreamEnd => Ok(Value::Null),
            FrameType::Error => Err(HistorianProducerError::Subc(error_body(&frame.body))),
            _ => Err(HistorianProducerError::UnexpectedControlResponse),
        }
    }

    async fn send_request(
        &mut self,
        channel: u16,
        body: Value,
    ) -> Result<u64, HistorianProducerError> {
        let corr = self.next_corr();
        let bytes = serde_json::to_vec(&body)?;
        self.write_frame(FrameType::Request, channel, corr, bytes)
            .await?;
        Ok(corr)
    }

    async fn drain_subscribe(
        &mut self,
        channel: u16,
        corr: u64,
        run_id: &str,
    ) -> Result<ProducerOutput, HistorianProducerError> {
        let mut text = String::new();
        let mut last_run_started: Option<String> = None;
        let mut length_capped = false;
        loop {
            let Some(frame) = read_frame(&mut self.stream).await? else {
                return Err(HistorianProducerError::UnexpectedStreamEnd);
            };
            if frame.header.channel != channel || frame.header.corr != corr {
                continue;
            }
            match frame.header.ty {
                FrameType::StreamData => {
                    let event: Value = serde_json::from_slice(&frame.body)?;
                    let Some(unit) = control_unit(&event) else {
                        continue;
                    };
                    if is_run_started_unit(unit) {
                        last_run_started = unit_run_id(unit).map(ToString::to_string);
                    }
                    let terminal = is_terminal_unit(unit);
                    if !terminal && unit_run_id(unit).is_some_and(|id| id != run_id) {
                        continue;
                    }
                    if is_paused_unit(unit) && unit_run_id(unit) == Some(run_id) {
                        // A paused run still holds the slot for this historian. Return
                        // an error so callers stop waiting and retry later instead of
                        // hanging forever on a run that is paused but not finished.
                        return Err(HistorianProducerError::RunPaused {
                            run_id: run_id.to_string(),
                            reason: paused_reason(unit).map(ToString::to_string),
                        });
                    }
                    if let Some(piece) = unit_text(unit) {
                        text.push_str(&piece);
                    }
                    if unit_is_length_capped(unit) {
                        length_capped = true;
                    }
                    if terminal {
                        if last_run_started.as_deref() != Some(run_id) {
                            return Err(HistorianProducerError::TerminalRunMismatch {
                                expected: run_id.to_string(),
                                found: last_run_started,
                            });
                        }
                        if is_error_unit(unit) {
                            return Err(HistorianProducerError::RunFailed {
                                run_id: run_id.to_string(),
                                detail: unit_error_detail(unit).unwrap_or("run failed").to_string(),
                            });
                        }
                        // The run terminal control unit is authoritative. StreamEnd is only
                        // route mechanics and can appear on detach/resubscribe without ending a run.
                        return Ok(ProducerOutput {
                            text,
                            length_capped,
                        });
                    }
                }
                FrameType::Error => {
                    return Err(HistorianProducerError::Subc(error_body(&frame.body)))
                }
                FrameType::StreamEnd => return Err(HistorianProducerError::UnexpectedStreamEnd),
                _ => {}
            }
        }
    }

    async fn read_terminal_for(
        &mut self,
        channel: u16,
        corr: u64,
        timeout: Duration,
    ) -> Result<Frame, HistorianProducerError> {
        match tokio::time::timeout(timeout, async {
            loop {
                let Some(frame) = read_frame(&mut self.stream).await? else {
                    return Err(HistorianProducerError::UnexpectedStreamEnd);
                };
                if frame.header.channel == channel && frame.header.corr == corr {
                    return Ok(frame);
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(HistorianProducerError::TimedOut),
        }
    }

    async fn write_frame(
        &mut self,
        ty: FrameType,
        channel: u16,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<(), HistorianProducerError> {
        let frame = Frame::build(
            ty,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            body,
        )?;
        write_frame(&mut self.stream, &frame).await?;
        Ok(())
    }

    async fn send_goodbye(&mut self, channel: u16) -> Result<(), HistorianProducerError> {
        let frame = Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Interactive, false),
            channel,
            0,
            Vec::new(),
        )?;
        write_frame(&mut self.stream, &frame).await?;
        Ok(())
    }

    fn next_corr(&mut self) -> u64 {
        let corr = self.next_corr;
        self.next_corr = self.next_corr.saturating_add(1).max(1);
        corr
    }
}

impl Drop for HistorianProducer {
    fn drop(&mut self) {
        // Async close is preferred by callers; drop only releases the TCP socket.
    }
}

fn classify_run_state(run_id: &str, value: &Value) -> RunState {
    let value = value.get("result").unwrap_or(value);
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let response_run_id = value.get("run_id").and_then(Value::as_str);
    if response_run_id.is_some_and(|id| id != run_id) {
        return RunState::Missing {
            detail: Some("run.status returned a different run_id".to_string()),
        };
    }
    if state.contains("terminal")
        || state.contains("interrupted")
        || state == "completed"
        || state == "finished"
    {
        RunState::Terminal
    } else if state.contains("active")
        || state.contains("paused")
        || state.contains("pending")
        || state.contains("running")
    {
        RunState::Active
    } else if state.contains("error") {
        RunState::Missing {
            detail: value
                .get("last_error")
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .map(ToString::to_string),
        }
    } else {
        RunState::Missing {
            detail: Some(state),
        }
    }
}

fn control_unit(event: &Value) -> Option<&Value> {
    let kind = event.get("kind").and_then(Value::as_str);
    if kind == Some("display") {
        return None;
    }
    if kind == Some("control") {
        let unit = event.get("unit").unwrap_or(event);
        return Some(unit);
    }
    Some(event.get("unit").unwrap_or(event))
}

fn unit_type(unit: &Value) -> Option<&str> {
    unit.get("type")
        .or_else(|| unit.get("kind"))
        .and_then(Value::as_str)
}

fn unit_run_id(unit: &Value) -> Option<&str> {
    unit.get("run_id")
        .or_else(|| unit.get("runId"))
        .and_then(Value::as_str)
}

/// Extract the assistant TEXT from a control unit. llm-runner's assistant_message
/// unit nests an assembled message with a content-block array; only `text` blocks are
/// the historian's output. `reasoning` blocks are deliberately EXCLUDED: a reasoning
/// model's thinking legitimately restates the prompt's format template and walks the
/// seed examples, so folding it into the output would corrupt the parse with
/// template/seed prose. Flat `text`/`content` fields are kept as a fallback for
/// simpler unit shapes.
fn unit_text(unit: &Value) -> Option<String> {
    if let Some(blocks) = unit
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        let text: String = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        return (!text.is_empty()).then_some(text);
    }
    unit.get("text")
        .or_else(|| unit.get("content"))
        .or_else(|| unit.get("message").and_then(|m| m.get("text")))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn is_terminal_unit(unit: &Value) -> bool {
    let Some(ty) = unit_type(unit).map(str::to_ascii_lowercase) else {
        return false;
    };
    ty == "run_finished"
        || ty == "terminal"
        || ty == "run_terminal"
        || ty == "finished"
        || ty == "error"
}

fn is_run_started_unit(unit: &Value) -> bool {
    unit_type(unit)
        .map(str::to_ascii_lowercase)
        .is_some_and(|ty| ty == "run_started" || ty == "runstarted")
}

/// A length-class finish reason on ANY unit (step or terminal): providers spell it
/// "length", "max_tokens", or "max_output_tokens" depending on the wire family.
fn unit_is_length_capped(unit: &Value) -> bool {
    unit.get("finish_reason")
        .or_else(|| unit.get("finishReason"))
        .and_then(Value::as_str)
        .is_some_and(|reason| {
            let reason = reason.to_ascii_lowercase();
            reason == "length" || reason == "max_tokens" || reason == "max_output_tokens"
        })
}

fn is_paused_unit(unit: &Value) -> bool {
    unit_type(unit)
        .map(str::to_ascii_lowercase)
        .is_some_and(|ty| ty == "paused")
}

fn paused_reason(unit: &Value) -> Option<&str> {
    unit.get("reason")
        .or_else(|| unit.get("detail"))
        .and_then(Value::as_str)
}

fn is_error_unit(unit: &Value) -> bool {
    unit_type(unit)
        .map(str::to_ascii_lowercase)
        .is_some_and(|ty| ty == "error" || ty == "run_error")
}

fn unit_error_detail(unit: &Value) -> Option<&str> {
    unit.get("detail")
        .or_else(|| unit.get("error"))
        .or_else(|| unit.get("message"))
        .and_then(Value::as_str)
}

fn consumer_identity_from_env() -> Option<ConsumerIdentity> {
    let module_id = std::env::var(SUBC_MODULE_ID_ENV).ok()?;
    let launch_nonce = std::env::var(SUBC_LAUNCH_NONCE_ENV).ok()?;
    (!module_id.is_empty() && !launch_nonce.is_empty()).then_some(ConsumerIdentity {
        module_id,
        launch_nonce,
    })
}

fn error_body(body: &[u8]) -> ErrorBody {
    serde_json::from_slice(body).unwrap_or_else(|e| ErrorBody {
        code: "invalid_error_body".to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, net::SocketAddr, sync::Arc};

    use serde_json::json;
    use subc_transport::{
        authenticate_server, generate_daemon_id, generate_key, write_atomic, ConnectionInfo,
        Endpoint, SCHEMA_VERSION,
    };
    use tempfile::TempDir;
    use tokio::{net::TcpListener, sync::Mutex};

    #[derive(Debug, Default)]
    struct ServerLog {
        route_sessions: Vec<String>,
        sends: Vec<Value>,
        subscribes: Vec<Value>,
        goodbyes: Vec<u16>,
    }

    struct FakeServer {
        connection_file: PathBuf,
        log: Arc<Mutex<ServerLog>>,
        _temp: TempDir,
    }

    async fn fake_server(send_response: Value, stream_events: Vec<Value>) -> FakeServer {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let key = generate_key().unwrap();
        let daemon_id = generate_daemon_id().unwrap();
        let connection_file = temp.path().join("subc-connection.json");
        write_atomic(
            &connection_file,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                endpoints: vec![Endpoint {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "fake".to_string(),
            },
        )
        .unwrap();
        let log = Arc::new(Mutex::new(ServerLog::default()));
        let log_task = Arc::clone(&log);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            authenticate_server(
                &mut stream,
                &key,
                &daemon_id,
                "fake",
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            let mut next_route = 10u16;
            let mut route_sessions = std::collections::HashMap::<u16, String>::new();
            let mut stream_events: VecDeque<Value> = stream_events.into();
            loop {
                let Some(frame) = read_frame(&mut stream).await.unwrap() else {
                    break;
                };
                match frame.header.ty {
                    FrameType::Goodbye => {
                        log_task.lock().await.goodbyes.push(frame.header.channel);
                    }
                    FrameType::Request if frame.header.channel == 0 => {
                        let req: ClientControlRequest =
                            serde_json::from_slice(&frame.body).unwrap();
                        if let ClientControlRequest::RouteOpen { identity, .. } = req {
                            let route = next_route;
                            next_route += 1;
                            route_sessions.insert(route, identity.session.clone());
                            log_task.lock().await.route_sessions.push(identity.session);
                            send_response_frame(
                                &mut stream,
                                frame.header.channel,
                                frame.header.corr,
                                serde_json::to_vec(&ClientControlResponse::RouteOpen {
                                    route_channel: route,
                                })
                                .unwrap(),
                            )
                            .await;
                        }
                    }
                    FrameType::Request => {
                        let req: Value = serde_json::from_slice(&frame.body).unwrap();
                        match req.get("method").and_then(Value::as_str) {
                            Some("session.send") => {
                                log_task.lock().await.sends.push(req["params"].clone());
                                send_response_frame(
                                    &mut stream,
                                    frame.header.channel,
                                    frame.header.corr,
                                    serde_json::to_vec(&send_response).unwrap(),
                                )
                                .await;
                            }
                            Some("session.subscribe") => {
                                log_task.lock().await.subscribes.push(req["params"].clone());
                                while let Some(event) = stream_events.pop_front() {
                                    send_stream_data(
                                        &mut stream,
                                        frame.header.channel,
                                        frame.header.corr,
                                        event,
                                    )
                                    .await;
                                }
                                send_stream_end(
                                    &mut stream,
                                    frame.header.channel,
                                    frame.header.corr,
                                )
                                .await;
                            }
                            Some("run.status") => {
                                send_response_frame(
                                    &mut stream,
                                    frame.header.channel,
                                    frame.header.corr,
                                    serde_json::to_vec(
                                        &json!({"state":"terminal","run_id":"run-1","head":"h"}),
                                    )
                                    .unwrap(),
                                )
                                .await;
                            }
                            Some("run.cancel") => {
                                send_response_frame(
                                    &mut stream,
                                    frame.header.channel,
                                    frame.header.corr,
                                    serde_json::to_vec(&json!({"ack":true})).unwrap(),
                                )
                                .await;
                            }
                            other => panic!(
                                "unexpected request {other:?} on route {:?}",
                                route_sessions.get(&frame.header.channel)
                            ),
                        }
                    }
                    _ => {}
                }
            }
        });
        FakeServer {
            connection_file,
            log,
            _temp: temp,
        }
    }

    async fn send_response_frame(stream: &mut TcpStream, channel: u16, corr: u64, body: Vec<u8>) {
        let frame = Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            body,
        )
        .unwrap();
        write_frame(stream, &frame).await.unwrap();
    }

    async fn send_stream_data(stream: &mut TcpStream, channel: u16, corr: u64, event: Value) {
        let frame = Frame::build(
            FrameType::StreamData,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            serde_json::to_vec(&event).unwrap(),
        )
        .unwrap();
        write_frame(stream, &frame).await.unwrap();
    }

    async fn send_stream_end(stream: &mut TcpStream, channel: u16, corr: u64) {
        let frame = Frame::build(
            FrameType::StreamEnd,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            Vec::new(),
        )
        .unwrap();
        write_frame(stream, &frame).await.unwrap();
    }

    async fn client(server: &FakeServer) -> HistorianProducer {
        HistorianProducer::connect(HistorianProducerConfig {
            connection_file: server.connection_file.clone(),
            project_root: std::env::current_dir().unwrap(),
            harness: "mc-test".to_string(),
            module_id: "llm-runner".to_string(),
            handshake_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(2),
            await_timeout: Duration::from_secs(2),
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn start_binds_session_at_route_open_and_omits_session_param() {
        let server = fake_server(json!({"state":"active","run_id":"run-1"}), Vec::new()).await;
        let mut client = client(&server).await;
        let handle = client
            .start(
                "mc-historian:proj:1",
                "role guidance",
                "prompt",
                "prov/model-a",
            )
            .await
            .unwrap();
        assert_eq!(handle.run_id, "run-1");
        client.close().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let log = server.log.lock().await;
        assert_eq!(log.route_sessions, vec!["mc-historian:proj:1"]);
        assert_eq!(log.sends.len(), 1);
        assert!(
            log.sends[0].get("session").is_none(),
            "session id lives in BindIdentity, not params"
        );
        assert_eq!(
            log.sends[0]["model"],
            json!({ "provider": "prov", "model": "model-a" }),
            "model is llm-runner's nested ModelParams object, split at the FIRST slash"
        );
        assert_eq!(log.sends[0]["tools"], json!([]));
        assert_eq!(
            log.sends[0]["generation"]["max_output_tokens"],
            json!(HISTORIAN_MAX_OUTPUT_TOKENS),
            "an explicit output budget rides every send: llm-runner's default truncated a real summarization pass"
        );
        assert_eq!(
            log.sends[0]["generation"]["temperature"],
            json!(HISTORIAN_TEMPERATURE),
            "the calibrated temperature rides every send: prompt and sampling were calibrated together"
        );
        assert_eq!(
            log.sends[0]["system"],
            json!("role guidance"),
            "system rides the role-scoped SendParams field, byte-exact"
        );
        assert_eq!(log.goodbyes, vec![10]);
    }

    #[tokio::test]
    async fn start_omits_system_param_when_empty() {
        // Empty means absent on the wire (the field's empty-as-absent rule); omitting it
        // entirely keeps the send byte-shape identical to pre-system clients.
        let server = fake_server(json!({"state":"active","run_id":"run-9"}), Vec::new()).await;
        let mut client = client(&server).await;
        client
            .start("mc-historian:proj:9", "", "prompt", "prov/model-a")
            .await
            .unwrap();
        client.close().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let log = server.log.lock().await;
        assert_eq!(log.sends.len(), 1);
        assert!(
            log.sends[0].get("system").is_none(),
            "empty system must be omitted, not sent as \"\""
        );
    }

    #[tokio::test]
    async fn await_output_uses_control_terminal_not_stream_end() {
        let terminal_text = r#"<output><compartments><compartment start="1" end="1" title="t"><p1>x</p1></compartment></compartments><meta><messages_processed>1-1</messages_processed></meta></output>"#;
        let events = vec![
            json!({"kind":"display","event":{"type":"text_delta","text":"ignored"}}),
            json!({"kind":"control","unit":{"type":"run_started","run_id":"run-1"}}),
            json!({"kind":"control","unit":{"type":"assistant_message","message":{"message_id":"m-1","content":[
                {"type":"reasoning","text":"planning prose that restates start=\"FIRST\" and walks the seed examples"},
                {"type":"text","text":terminal_text}
            ]}}}),
            json!({"kind":"control","unit":{"type":"run_finished","finish_reason":"completed"}}),
        ];
        let server = fake_server(json!({"state":"active","run_id":"run-1"}), events).await;
        let mut client = client(&server).await;
        client
            .start("mc-historian:proj:2", "", "prompt", "prov/model-a")
            .await
            .unwrap();
        let output = client.await_output("run-1").await.unwrap();
        client.close().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(output.text, terminal_text);
        let log = server.log.lock().await;
        assert_eq!(
            log.route_sessions,
            vec!["mc-historian:proj:2", "mc-historian:proj:2"]
        );
        assert_eq!(log.subscribes, vec![json!({"from":"start"})]);
        assert_eq!(
            log.goodbyes,
            vec![11, 10],
            "close releases subscribe and command routes"
        );
    }

    #[tokio::test]
    async fn terminal_without_matching_run_started_fails_loud() {
        let events = vec![
            json!({"kind":"control","unit":{"type":"run_started","run_id":"other-run"}}),
            json!({"kind":"control","unit":{"type":"run_finished","finish_reason":"completed"}}),
        ];
        let server = fake_server(json!({"state":"active","run_id":"run-1"}), events).await;
        let mut client = client(&server).await;
        client
            .start("mc-historian:proj:3", "", "prompt", "prov/model-a")
            .await
            .unwrap();

        let err = client.await_output("run-1").await.unwrap_err();
        assert!(matches!(
            err,
            HistorianProducerError::TerminalRunMismatch {
                expected,
                found: Some(found),
            } if expected == "run-1" && found == "other-run"
        ));
    }

    #[tokio::test]
    async fn paused_unit_returns_run_paused() {
        let events = vec![
            json!({"kind":"control","unit":{"type":"run_started","run_id":"run-1"}}),
            json!({"kind":"control","unit":{"type":"paused","run_id":"run-1","reason":"auth_required"}}),
        ];
        let server = fake_server(json!({"state":"active","run_id":"run-1"}), events).await;
        let mut client = client(&server).await;
        client
            .start("mc-historian:proj:4", "", "prompt", "prov/model-a")
            .await
            .unwrap();

        let err = client.await_output("run-1").await.unwrap_err();
        assert!(matches!(
            err,
            HistorianProducerError::RunPaused {
                run_id,
                reason: Some(reason),
            } if run_id == "run-1" && reason == "auth_required"
        ));
    }
}
