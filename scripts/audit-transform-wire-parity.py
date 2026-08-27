#!/usr/bin/env python3
"""Summarize structural evidence from anthropic-auth request wire dumps.

The audit intentionally compares serialized request bodies rather than trying to
reconstruct them from transform state. Rust-mode sessions are explicit because
that lane assignment is not encoded in the dump filename.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import hashlib
import json
import re
import sqlite3
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

RUST_SESSIONS = {
    "ses_0ad83017cffexe0g5N8UG0y3LZ",  # ENGRAM Rust transform session
    "ses_08df2045bffeBcWcqw60elghER",  # ASTROCYTE Rust transform session
    "ses_12a4fa38dffe81Fz7Y2AsWb5Cg",  # SUBCONSCIOUS configured Rust session
}
SESSION_PATTERN = re.compile(r"-(ses_[^-]+)-")
TAG_PATTERN = re.compile(r"^§(\d+)§(?P<separator> |$)")
ANY_TAG_PATTERN = re.compile(r"§(\d+)§")
TEMPORAL_PATTERN = re.compile(r"^<!-- \+[^>]+ -->\n")
DROP_PATTERN = re.compile(r"^\[dropped(?: §\d+§)?\]$")
SYSTEM_REMINDER_FULL_PATTERN = re.compile(
    r"^\s*<system-reminder>[\s\S]*</system-reminder>\s*$", re.IGNORECASE
)
CHANNEL_REMINDER_PATTERN = re.compile(
    r"(?P<reminder>(?:\n\n|^)<system-reminder>\n"
    r"(?P<body>(?:Housekeeping(?::| backlog:)|Reminder:|Routine housekeeping:)[^\n]*"
    r"(?:\noldest reclaimable: [^\n]*)?)"
    r"\n</system-reminder>)$"
)
CHANNEL1_DENOMINATOR_PATTERN = re.compile(
    r"~(?P<amount>\d+k) of this session's ~(?P<window>\d+k) window"
)
COMPARTMENT_HEADING_PATTERN = re.compile(
    r"^## \d+-\d+(?: · \d{4}-\d{2}-\d{2}(?: → \d{4}-\d{2}-\d{2})?)? · .+$"
)
M0_SECTION_NAMES = (
    "project-docs",
    "user-profile",
    "covered-system-messages",
    "session-history",
    "project-memory",
    "memory-mural",
)
M1_SECTION_NAMES = (
    "memory-updates",
    "new-compartments",
    "new-memories",
    "new-user-profile",
    "new-notes",
)
MAGIC_CONTEXT_MARKER = "## Magic Context"
DATE_LINE_PATTERN = re.compile(r"^\s*Today's date: .+$", re.MULTILINE)
TEMPORAL_TAG_PATTERN = re.compile(r"^<!-- \+[^>]+ -->\n§\d+§(?: |$)")
TAG_TEMPORAL_PATTERN = re.compile(r"^§\d+§ <!-- \+[^>]+ -->\n")
TRANSPORT_TEMPORAL_PATTERN = re.compile(
    r"^(?:§\d+§ )?<!-- \+[^>]+ -->\n\s*<system-reminder>"
)
M1_PLACEHOLDER_TEXT = "(no new content since last materialization)"
M1_PLACEHOLDER_WRAPPED = (
    "<session-history-since>"
    + M1_PLACEHOLDER_TEXT
    + "</session-history-since>"
)
COMPACTION_MARKER_PATTERN = re.compile(
    r"^(?:§(?P<tag>\d+)§ )?\[Compacted by magic-context — "
    r"session history is managed by the plugin\]$"
)
SEARCH_HINT_DROP_PATTERN = re.compile(
    r"^\[dropped §(?P<tag>\d+)§\]\n\n<ctx-search-hint>[\s\S]*</ctx-search-hint>$"
)
TRUNCATION_SENTINEL = "...[truncated]"
EDIT_TOOL_NAMES = {"edit", "write", "mcp_edit", "mcp_write"}
EDIT_DIFF_KEYS = {"content", "newstring", "oldstring", "patch", "diff"}
WORKING_DIRECTORY_PATTERN = re.compile(r"^Working directory: (?P<path>.+)$", re.MULTILINE)


@dataclass(frozen=True)
class Dump:
    path: Path
    session: str
    lane: str
    body: dict[str, Any]
    response: dict[str, Any] | None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("dump_dir", type=Path)
    parser.add_argument("--date", default=dt.datetime.now(dt.timezone.utc).date().isoformat())
    parser.add_argument("--per-session", type=int, default=6)
    parser.add_argument(
        "--after",
        help="include dump filenames at or after this UTC timestamp prefix",
    )
    parser.add_argument(
        "--before",
        help="include dump filenames at or before this UTC timestamp prefix",
    )
    parser.add_argument(
        "--rust-session",
        action="append",
        dest="rust_sessions",
        help="override the configured Rust session set (repeatable)",
    )
    parser.add_argument(
        "--context-db",
        type=Path,
        help="read-only context.db for caveman and transform-decision evidence",
    )
    parser.add_argument(
        "--store-db",
        type=Path,
        help="read-only Rust store.db for module activity and scheduler evidence",
    )
    parser.add_argument("--indent", type=int, default=2)
    return parser.parse_args()


def session_from_name(path: Path) -> str | None:
    match = SESSION_PATTERN.search(path.name)
    return match.group(1) if match else None


def choose_paths(
    root: Path,
    date: str,
    per_session: int,
    after: str | None = None,
    before: str | None = None,
) -> list[Path]:
    grouped: dict[str, list[Path]] = collections.defaultdict(list)
    for path in root.glob(f"{date}*.body.json"):
        if after is not None and path.name < after:
            continue
        if before is not None and path.name > before:
            continue
        session = session_from_name(path)
        if session is not None:
            grouped[session].append(path)
    return [
        path
        for session in sorted(grouped)
        for path in sorted(grouped[session])[-per_session:]
    ]


def load_dumps(paths: Iterable[Path], rust_sessions: set[str]) -> list[Dump]:
    dumps = []
    for path in paths:
        session = session_from_name(path)
        if session is None:
            continue
        response_path = path.with_name(path.name.replace(".body.json", ".response.json"))
        response = json.loads(response_path.read_text()) if response_path.exists() else None
        dumps.append(
            Dump(
                path=path,
                session=session,
                lane="rust" if session in rust_sessions else "ts",
                body=json.loads(path.read_text()),
                response=response,
            )
        )
    return dumps


def blocks(message: dict[str, Any]) -> list[dict[str, Any]]:
    content = message.get("content")
    if not isinstance(content, list):
        return []
    return [block for block in content if isinstance(block, dict)]


def text_fields(block: dict[str, Any]) -> Iterable[tuple[str, str]]:
    for key in ("text", "thinking"):
        value = block.get(key)
        if isinstance(value, str):
            yield key, value
    content = block.get("content")
    if isinstance(content, str):
        yield "content", content
    elif isinstance(content, list):
        for index, child in enumerate(content):
            if isinstance(child, dict) and isinstance(child.get("text"), str):
                yield f"content[{index}].text", child["text"]


def short(value: str, limit: int = 180) -> str:
    return value[:limit].replace("\n", "\\n")


def evidence(dump: Dump, message_index: int, block_index: int, value: str) -> dict[str, Any]:
    return {
        "session": dump.session,
        "file": dump.path.name,
        "message": message_index,
        "block": block_index,
        "excerpt": short(value),
    }


def counter_dict(
    counter: collections.Counter[Any], limit: int | None = None
) -> dict[str, int]:
    rows = sorted(counter.items(), key=lambda item: (-item[1], str(item[0])))
    if limit is not None:
        rows = rows[:limit]
    return {str(key): value for key, value in rows}


def json_paths(value: Any, path: str = "input") -> Iterable[tuple[str, Any]]:
    if isinstance(value, dict):
        for key, child in value.items():
            yield from json_paths(child, f"{path}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            yield from json_paths(child, f"{path}[{index}]")
    else:
        yield path, value


def raw_hash(value: Any) -> str:
    encoded = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()[:16]


def text_hash(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()[:16]


def starts_section(value: str, name: str) -> bool:
    return re.match(rf"^<{re.escape(name)}(?:>| )", value) is not None


def section_start(value: str, name: str, offset: int = 0) -> int:
    match = re.search(rf"(?m)^<{re.escape(name)}(?:>| )", value[offset:])
    return -1 if match is None else offset + match.start()


def classify_text_field(
    role: Any,
    block_type: Any,
    field: str,
    value: str,
    message_index: int,
) -> str:
    if role == "user" and message_index == 0 and block_type == "text":
        if any(starts_section(value, name) for name in M0_SECTION_NAMES):
            return "synthetic_m0"
        if value.startswith("<session-history-since>"):
            return "synthetic_m1"
    if role == "user" and block_type == "tool_result":
        return "tool_result_text"
    if role == "user" and block_type == "text":
        body = TAG_PATTERN.sub("", value, count=1)
        body = TEMPORAL_PATTERN.sub("", body, count=1)
        if SYSTEM_REMINDER_FULL_PATTERN.fullmatch(body):
            return "user_transport_reminder"
        return "user_text"
    if role == "assistant" and block_type == "text":
        return "assistant_text"
    if block_type in ("thinking", "reasoning", "redacted_thinking"):
        return "assistant_reasoning"
    return f"{role}_{block_type}_{field}"


def tag_placement(value: str) -> tuple[str, int | None]:
    prefix = TAG_PATTERN.match(value)
    if prefix:
        separator = "space" if prefix.group("separator") == " " else "eof"
        return f"prefix_{separator}", len(prefix.group(1))
    if ANY_TAG_PATTERN.search(value):
        return "embedded_or_suffix", None
    return "absent", None


def ordered_sections(value: str, names: tuple[str, ...]) -> tuple[str, ...]:
    positions = [
        (position, name)
        for name in names
        if (position := section_start(value, name)) >= 0
    ]
    return tuple(name for _, name in sorted(positions))


def section_separators(value: str, order: tuple[str, ...]) -> tuple[str, ...]:
    separators: list[str] = []
    search_offset = 0
    for left, right in zip(order, order[1:]):
        left_start = section_start(value, left, search_offset)
        close = f"</{left}>"
        left_end = value.find(close, left_start)
        right_start = section_start(value, right, left_end + len(close))
        if left_start < 0 or left_end < 0 or right_start < 0:
            separators.append("unresolved")
            continue
        between = value[left_end + len(close) : right_start]
        separators.append(repr(between))
        search_offset = right_start
    return tuple(separators)


def section_body(value: str, name: str) -> str | None:
    start = section_start(value, name)
    if start < 0:
        return None
    opener_end = value.find(">", start)
    close_start = value.find(f"</{name}>", opener_end + 1)
    if opener_end < 0 or close_start < 0:
        return None
    return value[opener_end + 1 : close_start]


def normalized_channel1_template(reminder: str) -> str:
    normalized = re.sub(r"~\d+k", "~<tokens>", reminder)
    normalized = re.sub(r"\b\d+ spent tool outputs?\b", "<count> spent tool outputs", normalized)
    normalized = re.sub(
        r"\noldest reclaimable: [^\n]*\.(?=\n</system-reminder>)",
        "\noldest reclaimable: <hint>.",
        normalized,
    )
    return normalized


def reminder_shape(value: str) -> tuple[str, str, str, bool, str] | None:
    match = CHANNEL_REMINDER_PATTERN.search(value)
    if match is None:
        return None
    body = match.group("body")
    first_line = body.splitlines()[0]
    if first_line.startswith("Routine housekeeping:"):
        channel = "channel2"
        band = "ceiling"
    elif first_line.startswith("Reminder:"):
        channel = "channel1"
        band = "sticky"
    elif first_line.startswith("Housekeeping backlog:"):
        channel = "channel1"
        band = "urgent"
    elif "drop the ones" in first_line or "some earlier tool outputs" in first_line:
        channel = "channel1"
        band = "gentle"
    else:
        channel = "channel1"
        band = "firm"
    version = "degauged" if " are reclaimable —" in first_line else "gauged"
    placement = "full" if match.start("reminder") == 0 else "suffix"
    return channel, band, version, "\noldest reclaimable:" in body, placement


def tool_reduction_shape(name: Any, tool_input: dict[str, Any]) -> str:
    if "reduced" in tool_input or "summary" in tool_input:
        return "reduced_envelope"
    normalized_name = str(name).lower()
    for key, value in tool_input.items():
        if (
            normalized_name in EDIT_TOOL_NAMES
            and key.lower() in EDIT_DIFF_KEYS
            and isinstance(value, str)
            and TRUNCATION_SENTINEL in value
        ):
            return "edit_marker"
    if any(
        isinstance(value, str) and TRUNCATION_SENTINEL in value
        for _, value in json_paths(tool_input)
    ):
        return "skeleton"
    return "full_or_small"


def project_root(body: dict[str, Any]) -> str | None:
    system = body.get("system")
    if not isinstance(system, list):
        return None
    for item in system:
        if not isinstance(item, dict) or not isinstance(item.get("text"), str):
            continue
        match = WORKING_DIRECTORY_PATTERN.search(item["text"])
        if match:
            return match.group("path")
    return None


def media_shape(message_index: int, role: Any, block: dict[str, Any]) -> tuple[Any, ...] | None:
    block_type = block.get("type")
    if block_type not in ("image", "document", "file", "attachment"):
        return None
    source = block.get("source")
    source_keys = tuple(sorted(source)) if isinstance(source, dict) else ()
    location = "m0_mural" if message_index == 0 and block_type == "image" else "transcript"
    return (
        location,
        role,
        block_type,
        tuple(sorted(block)),
        source.get("type") if isinstance(source, dict) else None,
        source.get("media_type") if isinstance(source, dict) else None,
        source_keys,
    )


def parse_bound(value: str | None, date: str, end: bool = False) -> int:
    if value:
        filename_stamp = re.match(
            r"^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z", value
        )
        stamp = filename_stamp.group(0) if filename_stamp else value
        formats = (
            "%Y-%m-%dT%H-%M-%S-%fZ",
            "%Y-%m-%dT%H-%M-%S",
            "%Y-%m-%dT%H-%M",
            "%Y-%m-%dT%H",
        )
        for format_string in formats:
            try:
                parsed = dt.datetime.strptime(stamp, format_string).replace(
                    tzinfo=dt.timezone.utc
                )
                return int(parsed.timestamp() * 1000)
            except ValueError:
                continue
        raise ValueError(f"unsupported timestamp bound: {value}")
    day = dt.datetime.strptime(date, "%Y-%m-%d").replace(tzinfo=dt.timezone.utc)
    if end:
        day += dt.timedelta(days=1)
    return int(day.timestamp() * 1000)


def summarize_telemetry(
    context_db: Path | None,
    store_db: Path | None,
    sessions: set[str],
    configured_rust_sessions: set[str],
    start_ms: int,
    end_ms: int,
) -> dict[str, Any]:
    report: dict[str, Any] = {}
    transform_rows: dict[str, list[tuple[Any, ...]]] = collections.defaultdict(list)
    if context_db is not None:
        with sqlite3.connect(f"file:{context_db}?mode=ro", uri=True) as db:
            placeholders = ",".join("?" for _ in sessions)
            if sessions:
                rows = db.execute(
                    f"""SELECT session_id, ts_ms, decision, materialize_reason,
                               input_tokens, emergency, dropped_count,
                               system_hash_prev, system_hash_new,
                               m0_model_key_prev, m0_model_key_new,
                               m0_tool_set_hash_prev, m0_tool_set_hash_new
                          FROM transform_decisions
                         WHERE session_id IN ({placeholders}) AND ts_ms >= ? AND ts_ms < ?
                         ORDER BY ts_ms""",
                    (*sorted(sessions), start_ms, end_ms),
                ).fetchall()
                for row in rows:
                    transform_rows[str(row[0])].append(tuple(row[1:]))
            caveman_rows = db.execute(
                """SELECT session_id, caveman_depth, COUNT(*), MIN(tag_number), MAX(tag_number)
                       FROM tags WHERE caveman_depth > 0
                      GROUP BY session_id, caveman_depth ORDER BY session_id, caveman_depth"""
            ).fetchall()
        report["caveman_depth_state"] = [
            {
                "session": row[0],
                "depth": row[1],
                "count": row[2],
                "min_tag": row[3],
                "max_tag": row[4],
            }
            for row in caveman_rows
            if row[0] in sessions
        ]

    module_activity: dict[str, dict[str, Any]] = {}
    if store_db is not None:
        with sqlite3.connect(f"file:{store_db}?mode=ro", uri=True) as db:
            placeholders = ",".join("?" for _ in sessions)
            if sessions:
                for session, last_activity, meta in db.execute(
                    f"SELECT session_id, last_activity_at, meta FROM mc_cache_state WHERE session_id IN ({placeholders})",
                    tuple(sorted(sessions)),
                ):
                    parsed_meta = json.loads(meta)
                    module_activity[str(session)] = {
                        "last_activity_at_ms": last_activity,
                        "last_activity_in_window": start_ms <= last_activity < end_ms,
                        "active_since_window_start": last_activity >= start_ms,
                        "caveman_age_basis_tag": parsed_meta.get("caveman_age_basis_tag"),
                    }
                scheduler_rows = db.execute(
                    f"SELECT session_id, scheduler_history, scheduler_interesting_history FROM mc_pass_trace WHERE session_id IN ({placeholders})",
                    tuple(sorted(sessions)),
                ).fetchall()
            else:
                scheduler_rows = []
        scheduler_report: dict[str, Any] = {}
        for session, history_json, interesting_json in scheduler_rows:
            history = [
                row
                for row in json.loads(history_json)
                if start_ms <= row.get("timestamp_ms", -1) < end_ms
            ]
            interesting = [
                row
                for row in json.loads(interesting_json)
                if start_ms <= row.get("timestamp_ms", -1) < end_ms
            ]
            scheduler_report[str(session)] = {
                "decisions": counter_dict(
                    collections.Counter(row.get("scheduler_decision") for row in history)
                ),
                "interesting_decisions": counter_dict(
                    collections.Counter(row.get("scheduler_decision") for row in interesting)
                ),
                "interesting_predicates": counter_dict(
                    collections.Counter(
                        (
                            row.get("scheduler_decision"),
                            row.get("drain_latch_active"),
                            row.get("eligible_supersession_count"),
                            row.get("applied_supersession_count"),
                            row.get("withheld_by_tag_window"),
                            row.get("withheld_by_exempt_message"),
                        )
                        for row in interesting
                    )
                ),
            }
        report["rust_scheduler"] = scheduler_report
        report["module_activity"] = module_activity

    decision_report: dict[str, Any] = {}
    matched_bands: dict[str, collections.Counter[Any]] = {
        "rust": collections.Counter(),
        "ts": collections.Counter(),
    }
    for session in sorted(sessions):
        rows = transform_rows.get(session, [])
        operand_rows = sum(any(value is not None for value in row[6:]) for row in rows)
        module_active = module_activity.get(session, {}).get(
            "active_since_window_start", False
        )
        observed_lane = (
            "rust"
            if module_active and operand_rows == 0
            else "ts"
            if operand_rows > 0
            else "unknown"
        )
        decisions = collections.Counter(row[1] for row in rows)
        reasons = collections.Counter(row[2] for row in rows)
        decision_report[session] = {
            "configured_lane": "rust" if session in configured_rust_sessions else "ts",
            "observed_lane": observed_lane,
            "rows": len(rows),
            "rows_with_ts_only_operands": operand_rows,
            "module_active_since_window_start": module_active,
            "decisions": counter_dict(decisions),
            "materialize_reasons": counter_dict(reasons),
        }
        for row in rows:
            input_band = f"{int(row[3]) // 50_000 * 50}k-{(int(row[3]) // 50_000 + 1) * 50}k"
            if observed_lane in matched_bands:
                matched_bands[observed_lane][
                    (input_band, row[1], row[2], bool(row[4]), row[5])
                ] += 1
    report["transform_decisions_by_session"] = decision_report
    report["matched_input_predicates"] = {
        lane: counter_dict(counter) for lane, counter in matched_bands.items()
    }
    return report


def summarize_lane(dumps: list[Dump]) -> dict[str, Any]:
    sessions = collections.Counter(dump.session for dump in dumps)
    response_statuses: collections.Counter[Any] = collections.Counter()
    response_usage_bands: collections.Counter[Any] = collections.Counter()
    max_response_total_input = -1
    max_response_usage: dict[str, Any] | None = None
    system_shapes: collections.Counter[Any] = collections.Counter()
    system_compositions: collections.Counter[Any] = collections.Counter()
    guidance_suffixes: collections.Counter[Any] = collections.Counter()
    head_shapes: collections.Counter[Any] = collections.Counter()
    m0_section_orders: collections.Counter[Any] = collections.Counter()
    m0_section_separators: collections.Counter[Any] = collections.Counter()
    m1_section_orders: collections.Counter[Any] = collections.Counter()
    m1_section_separators: collections.Counter[Any] = collections.Counter()
    compartment_heading_shapes: collections.Counter[Any] = collections.Counter()
    assistant_orders: collections.Counter[Any] = collections.Counter()
    trailing_shapes: collections.Counter[Any] = collections.Counter()
    text_classes: collections.Counter[Any] = collections.Counter()
    tag_classes: collections.Counter[Any] = collections.Counter()
    tag_placements: collections.Counter[Any] = collections.Counter()
    tag_prefix_formats: collections.Counter[Any] = collections.Counter()
    temporal_classes: collections.Counter[Any] = collections.Counter()
    temporal_tag_orders: collections.Counter[Any] = collections.Counter()
    reminder_temporal_classes: collections.Counter[Any] = collections.Counter()
    channel1_reminder_shapes: collections.Counter[Any] = collections.Counter()
    channel1_template_shapes: collections.Counter[Any] = collections.Counter()
    channel1_denominators: collections.Counter[Any] = collections.Counter()
    m1_placeholders: collections.Counter[Any] = collections.Counter()
    tool_input_shapes: collections.Counter[Any] = collections.Counter()
    reduced_envelopes: collections.Counter[Any] = collections.Counter()
    tool_special_values: collections.Counter[Any] = collections.Counter()
    tool_reduction_arc_shapes: collections.Counter[Any] = collections.Counter()
    tool_reduction_vocabulary: collections.Counter[Any] = collections.Counter()
    skeleton_recency: collections.Counter[Any] = collections.Counter()
    placeholder_values: collections.Counter[Any] = collections.Counter()
    drop_shapes: collections.Counter[Any] = collections.Counter()
    compaction_marker_shapes: collections.Counter[Any] = collections.Counter()
    nudge_assembly_shapes: collections.Counter[Any] = collections.Counter()
    media_shapes: collections.Counter[Any] = collections.Counter()
    project_roots: collections.Counter[Any] = collections.Counter()
    thinking_shapes: collections.Counter[Any] = collections.Counter()
    reasoning_order_shapes: collections.Counter[Any] = collections.Counter()
    newest_assistant_reasoning_presence: collections.Counter[Any] = collections.Counter()
    newest_assistant_reasoning_shapes: collections.Counter[Any] = collections.Counter()
    cache_placements: collections.Counter[Any] = collections.Counter()
    message_block_key_shapes: collections.Counter[Any] = collections.Counter()
    anomalies: collections.Counter[Any] = collections.Counter()
    special_evidence: dict[str, list[dict[str, Any]]] = collections.defaultdict(list)
    todo_observations: dict[str, list[tuple[str, int, str]]] = collections.defaultdict(list)

    for dump in dumps:
        response_status = (
            dump.response.get("status") if isinstance(dump.response, dict) else "missing"
        )
        response_statuses[response_status] += 1
        if response_status != 200 and len(special_evidence["non_200_response"]) < 12:
            special_evidence["non_200_response"].append(
                evidence(dump, -1, -1, f"response={dump.response}")
            )
        response_usage = (
            dump.response.get("usage")
            if isinstance(dump.response, dict) and isinstance(dump.response.get("usage"), dict)
            else None
        )
        if response_usage is not None:
            total_input = sum(
                value
                for key in (
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                )
                if isinstance((value := response_usage.get(key)), int)
            )
            if total_input >= 900_000:
                usage_band = ">=900k"
            elif total_input >= 750_000:
                usage_band = "750k-900k"
            elif total_input >= 500_000:
                usage_band = "500k-750k"
            elif total_input >= 250_000:
                usage_band = "250k-500k"
            else:
                usage_band = "<250k"
            response_usage_bands[usage_band] += 1
            if total_input > max_response_total_input:
                max_response_total_input = total_input
                max_response_usage = evidence(
                    dump,
                    -1,
                    -1,
                    f"total_input={total_input} usage={response_usage}",
                )
            if total_input >= 750_000 and len(special_evidence["high_pressure_usage"]) < 12:
                special_evidence["high_pressure_usage"].append(
                    evidence(
                        dump,
                        -1,
                        -1,
                        f"total_input={total_input} usage={response_usage}",
                    )
                )

        body = dump.body
        root = project_root(body)
        if root is not None:
            project_roots[(dump.session, root)] += 1
        messages = body.get("messages") if isinstance(body.get("messages"), list) else []
        system = body.get("system")
        if isinstance(system, list):
            system_shapes[
                (
                    len(system),
                    tuple(tuple(sorted(item)) for item in system if isinstance(item, dict)),
                    tuple(
                        index
                        for index, item in enumerate(system)
                        if isinstance(item, dict) and "cache_control" in item
                    ),
                )
            ] += 1
            guidance_indexes: list[int] = []
            date_indexes: list[int] = []
            guidance_separator_shapes: list[str] = []
            for index, item in enumerate(system):
                if not isinstance(item, dict):
                    continue
                if "cache_control" in item:
                    cache_placements[("system", index, item.get("type"))] += 1
                text = item.get("text")
                if not isinstance(text, str):
                    continue
                marker_index = text.find(MAGIC_CONTEXT_MARKER)
                if marker_index >= 0:
                    guidance_indexes.append(index)
                    separator = text[max(0, marker_index - 2) : marker_index]
                    guidance_separator_shapes.append(repr(separator))
                    suffix = text[marker_index:]
                    suffix_hash = text_hash(suffix)
                    guidance_suffixes[(index, suffix_hash, len(suffix), short(suffix, 120))] += 1
                    if len(special_evidence["system_guidance"]) < 6:
                        special_evidence["system_guidance"].append(
                            evidence(
                                dump,
                                -1,
                                index,
                                text[max(0, marker_index - 2) : marker_index + 220],
                            )
                        )
                if DATE_LINE_PATTERN.search(text):
                    date_indexes.append(index)
            system_compositions[
                (
                    len(system),
                    tuple(guidance_indexes),
                    tuple(date_indexes),
                    tuple(guidance_separator_shapes),
                    sum(
                        item.get("text", "").count(MAGIC_CONTEXT_MARKER)
                        for item in system
                        if isinstance(item, dict) and isinstance(item.get("text"), str)
                    ),
                )
            ] += 1
        else:
            system_shapes[(type(system).__name__,)] += 1

        head_shapes[
            tuple(
                (
                    message.get("role"),
                    tuple(block.get("type") for block in blocks(message)),
                )
                for message in messages[:4]
                if isinstance(message, dict)
            )
        ] += 1
        if messages and isinstance(messages[0], dict):
            for block_index, block in enumerate(blocks(messages[0])):
                value = block.get("text")
                if not isinstance(value, str):
                    continue
                if value == M1_PLACEHOLDER_TEXT:
                    m1_placeholders[("bare", block_index)] += 1
                elif value == M1_PLACEHOLDER_WRAPPED:
                    m1_placeholders[("wrapped", block_index)] += 1
                heading_bodies: list[str] = []
                if any(starts_section(value, name) for name in M0_SECTION_NAMES):
                    order = ordered_sections(value, M0_SECTION_NAMES)
                    m0_section_orders[(block_index, order)] += 1
                    m0_section_separators[(order, section_separators(value, order))] += 1
                    history_body = section_body(value, "session-history")
                    if history_body is not None:
                        heading_bodies.append(history_body)
                    if len(special_evidence["m0_layout"]) < 6:
                        special_evidence["m0_layout"].append(
                            evidence(dump, 0, block_index, value)
                        )
                if starts_section(value, "session-history-since"):
                    order = ordered_sections(value, M1_SECTION_NAMES)
                    m1_section_orders[(block_index, order or ("placeholder",))] += 1
                    if order:
                        m1_section_separators[(order, section_separators(value, order))] += 1
                    new_compartments = section_body(value, "new-compartments")
                    if new_compartments is not None:
                        heading_bodies.append(new_compartments)
                    if len(special_evidence["m1_layout"]) < 6:
                        special_evidence["m1_layout"].append(
                            evidence(dump, 0, block_index, value)
                        )
                for heading_body in heading_bodies:
                    for line in heading_body.splitlines():
                        if not line.startswith("## "):
                            continue
                        compartment_heading_shapes[
                            "valid" if COMPARTMENT_HEADING_PATTERN.fullmatch(line) else "invalid"
                        ] += 1
        trailing_shapes[
            tuple(
                (
                    message.get("role"),
                    tuple(block.get("type") for block in blocks(message)),
                )
                for message in messages[-4:]
                if isinstance(message, dict)
            )
        ] += 1

        newest_assistant = next(
            (
                (message_index, message)
                for message_index, message in reversed(list(enumerate(messages)))
                if isinstance(message, dict) and message.get("role") == "assistant"
            ),
            None,
        )
        if newest_assistant is None:
            newest_assistant_reasoning_presence["missing_assistant"] += 1
            newest_assistant_reasoning_shapes[("missing_assistant",)] += 1
        else:
            message_index, message = newest_assistant
            message_blocks = blocks(message)
            types = tuple(block.get("type") for block in message_blocks)
            reasoning_blocks = [
                (block_index, block)
                for block_index, block in enumerate(message_blocks)
                if block.get("type") in ("thinking", "reasoning", "redacted_thinking")
            ]
            presence = "present" if reasoning_blocks else "absent"
            signed_count = sum(bool(block.get("signature")) for _, block in reasoning_blocks)
            newest_assistant_reasoning_presence[presence] += 1
            newest_assistant_reasoning_shapes[
                (presence, types, len(reasoning_blocks), signed_count)
            ] += 1
            evidence_key = f"newest_assistant_reasoning_{presence}"
            if len(special_evidence[evidence_key]) < 6:
                if reasoning_blocks:
                    block_index, block = reasoning_blocks[0]
                    value = block.get("thinking", block.get("text", block.get("data", "")))
                    special_evidence[evidence_key].append(
                        evidence(dump, message_index, block_index, str(value))
                    )
                else:
                    special_evidence[evidence_key].append(
                        evidence(dump, message_index, -1, f"types={types}")
                    )

        tool_ids: collections.Counter[str] = collections.Counter()
        result_ids: collections.Counter[str] = collections.Counter()
        tool_calls: dict[str, tuple[str, str, int, int]] = {}
        tool_call_order: list[str] = []
        tool_result_shapes: dict[str, str] = {}
        previous_role = None
        for message_index, message in enumerate(messages):
            if not isinstance(message, dict):
                anomalies["non_object_message"] += 1
                continue
            role = message.get("role")
            if role == previous_role:
                anomalies[f"adjacent_role:{role}"] += 1
            previous_role = role
            message_blocks = blocks(message)
            if not message_blocks:
                anomalies[f"empty_content:{role}"] += 1
            types = tuple(block.get("type") for block in message_blocks)
            if role == "assistant":
                assistant_orders[types] += 1
                if "thinking" in types or "reasoning" in types:
                    reasoning_order_shapes[types] += 1

            for block_index, block in enumerate(message_blocks):
                block_type = block.get("type")
                message_block_key_shapes[(role, block_type, tuple(sorted(block)))] += 1
                shape = media_shape(message_index, role, block)
                if shape is not None:
                    media_shapes[shape] += 1
                    if len(special_evidence["media"]) < 12:
                        special_evidence["media"].append(
                            evidence(dump, message_index, block_index, str(shape))
                        )
                if "cache_control" in block:
                    cache_placements[("message", role, block_type)] += 1
                if block_type == "tool_use":
                    tool_id = block.get("id")
                    if isinstance(tool_id, str):
                        tool_ids[tool_id] += 1
                    tool_input = block.get("input")
                    if isinstance(tool_input, dict):
                        tool_name = block.get("name")
                        tool_input_shapes[(tool_name, tuple(sorted(tool_input)))] += 1
                        input_shape = tool_reduction_shape(tool_name, tool_input)
                        if input_shape == "reduced_envelope":
                            reduced_envelopes[
                                (
                                    tool_name,
                                    tuple(sorted(tool_input)),
                                    type(tool_input.get("reduced")).__name__,
                                    type(tool_input.get("summary")).__name__,
                                )
                            ] += 1
                        for path, value in json_paths(tool_input):
                            if isinstance(value, str) and TRUNCATION_SENTINEL in value:
                                key = (tool_name, path, TRUNCATION_SENTINEL)
                                tool_special_values[key] += 1
                                prefix = value.split(TRUNCATION_SENTINEL, 1)[0]
                                tool_reduction_vocabulary[
                                    (input_shape, tool_name, path, len(prefix), TRUNCATION_SENTINEL)
                                ] += 1
                                if len(special_evidence[f"tool_input_{input_shape}"]) < 12:
                                    special_evidence[f"tool_input_{input_shape}"].append(
                                        evidence(dump, message_index, block_index, value)
                                    )
                        if isinstance(tool_id, str):
                            tool_calls[tool_id] = (
                                str(tool_name),
                                input_shape,
                                message_index,
                                block_index,
                            )
                            tool_call_order.append(tool_id)
                    if (
                        isinstance(tool_id, str)
                        and tool_id.startswith("mc_synthetic_todo_")
                        and message_index + 1 < len(messages)
                    ):
                        pair = [message, messages[message_index + 1]]
                        todo_observations[tool_id].append(
                            (dump.path.name, message_index, raw_hash(pair))
                        )
                elif block_type == "tool_result":
                    tool_id = block.get("tool_use_id")
                    if isinstance(tool_id, str):
                        result_ids[tool_id] += 1
                        result_values = [value for _, value in text_fields(block)]
                        if any(
                            (DROP_PATTERN.fullmatch(value) and "§" in value)
                            or SEARCH_HINT_DROP_PATTERN.fullmatch(value)
                            for value in result_values
                        ):
                            tool_result_shapes[tool_id] = "tagged_dropped"
                        elif any(DROP_PATTERN.fullmatch(value) for value in result_values):
                            tool_result_shapes[tool_id] = "bare_dropped"
                        else:
                            tool_result_shapes[tool_id] = "visible"

                if block_type in ("thinking", "reasoning"):
                    thinking_shapes[
                        (
                            block_type,
                            tuple(sorted(block)),
                            bool(block.get("signature")),
                            "nonempty" if block.get("thinking", block.get("text", "")) else "empty",
                        )
                    ] += 1

                for field, value in text_fields(block):
                    text_class = classify_text_field(
                        role, block_type, field, value, message_index
                    )
                    text_classes[text_class] += 1
                    placement, digit_width = tag_placement(value)
                    tag_placements[(text_class, placement)] += 1
                    if digit_width is not None:
                        prefix_shape = "§<digits>§ " if placement == "prefix_space" else "§<digits>§"
                        tag_prefix_formats[(text_class, prefix_shape, digit_width)] += 1
                    if (
                        text_class
                        in (
                            "assistant_text",
                            "user_text",
                            "user_transport_reminder",
                            "tool_result_text",
                        )
                        and placement != "prefix_space"
                        and not DROP_PATTERN.fullmatch(value)
                    ):
                        evidence_key = f"tag_scope_{text_class}_{placement}"
                        if len(special_evidence[evidence_key]) < 6:
                            special_evidence[evidence_key].append(
                                evidence(dump, message_index, block_index, value)
                            )

                    tag = TAG_PATTERN.match(value)
                    if tag:
                        tag_classes[(role, block_type, field)] += 1
                        if len(special_evidence["tag"]) < 6:
                            special_evidence["tag"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    temporal = TEMPORAL_PATTERN.match(value)
                    tag_temporal = TAG_TEMPORAL_PATTERN.match(value)
                    if text_class == "user_transport_reminder":
                        reminder_temporal_classes[
                            ("user_transport_reminder", "present" if temporal or tag_temporal else "absent")
                        ] += 1
                    if temporal or tag_temporal:
                        temporal_classes[(role, block_type, field)] += 1
                        if TEMPORAL_TAG_PATTERN.match(value):
                            temporal_tag_orders["temporal_then_tag"] += 1
                        elif TAG_TEMPORAL_PATTERN.match(value):
                            temporal_tag_orders["tag_then_temporal"] += 1
                        else:
                            temporal_tag_orders["temporal_without_leading_tag"] += 1
                        if TRANSPORT_TEMPORAL_PATTERN.match(value):
                            temporal_tag_orders["standalone_transport"] += 1
                        if len(special_evidence["temporal"]) < 12:
                            special_evidence["temporal"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    reminder_match = CHANNEL_REMINDER_PATTERN.search(value)
                    reminder = reminder_shape(value)
                    if reminder_match and reminder:
                        channel, band, version, hint_present, span = reminder
                        reminder_text = reminder_match.group("reminder")
                        body = reminder_match.group("body")
                        nudge_assembly_shapes[
                            (
                                channel,
                                band,
                                version,
                                role,
                                block_type,
                                types,
                                span,
                                "hint" if hint_present else "no_hint",
                            )
                        ] += 1
                        template = normalized_channel1_template(reminder_text)
                        channel1_reminder_shapes[
                            (
                                channel,
                                band,
                                version,
                                text_hash(reminder_text),
                                len(reminder_text),
                                short(reminder_text, 260),
                            )
                        ] += 1
                        channel1_template_shapes[
                            (channel, band, version, text_hash(template), template)
                        ] += 1
                        denominator = CHANNEL1_DENOMINATOR_PATTERN.search(body)
                        if denominator:
                            channel1_denominators[
                                (
                                    band,
                                    denominator.group("amount"),
                                    denominator.group("window"),
                                )
                            ] += 1
                        evidence_key = f"{channel}_{band}_{version}"
                        if len(special_evidence[evidence_key]) < 6:
                            special_evidence[evidence_key].append(
                                evidence(dump, message_index, block_index, reminder_text)
                            )
                    search_hint_drop = SEARCH_HINT_DROP_PATTERN.fullmatch(value)
                    if DROP_PATTERN.fullmatch(value) or search_hint_drop:
                        drop_shape = (
                            "tagged_dropped_with_search_hint"
                            if search_hint_drop
                            else "tagged_dropped"
                            if "§" in value
                            else "bare_dropped"
                        )
                        placeholder_values[drop_shape] += 1
                        drop_shapes[(text_class, drop_shape)] += 1
                        if len(special_evidence["drop"]) < 12:
                            special_evidence["drop"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    compaction = COMPACTION_MARKER_PATTERN.fullmatch(value)
                    if compaction:
                        placeholder_values["compaction_summary"] += 1
                        compaction_marker_shapes[
                            (
                                role,
                                block_type,
                                types,
                                block_index,
                                "tagged" if compaction.group("tag") else "untagged",
                                COMPACTION_MARKER_PATTERN.sub(
                                    "§<tag>§ [Compacted by magic-context — session history is managed by the plugin]",
                                    value,
                                ),
                            )
                        ] += 1
                        if len(special_evidence["compaction_marker"]) < 12:
                            special_evidence["compaction_marker"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    if "...[truncated]" in value:
                        placeholder_values["...[truncated]"] += 1
                    elif "truncated" in value.lower():
                        placeholder_values["other_truncated_text"] += 1

        for call_index, tool_id in enumerate(tool_call_order):
            name, input_shape, message_index, block_index = tool_calls[tool_id]
            result_shape = tool_result_shapes.get(tool_id, "missing")
            if input_shape == "full_or_small" and result_shape not in (
                "tagged_dropped",
                "bare_dropped",
            ):
                continue
            newer_tool_calls = len(tool_call_order) - call_index - 1
            tool_reduction_arc_shapes[(name, input_shape, result_shape)] += 1
            if input_shape in ("skeleton", "edit_marker"):
                recency_band = "newest_20" if newer_tool_calls < 20 else "older_replay"
                skeleton_recency[(recency_band, newer_tool_calls)] += 1
                if len(special_evidence["skeleton"]) < 12:
                    special_evidence["skeleton"].append(
                        evidence(
                            dump,
                            message_index,
                            block_index,
                            f"tool={name} representation={input_shape} newer_tool_calls={newer_tool_calls} result={result_shape}",
                        )
                    )

        anomalies["duplicate_tool_use_ids"] += sum(count - 1 for count in tool_ids.values() if count > 1)
        anomalies["orphan_tool_results"] += sum(
            count for tool_id, count in result_ids.items() if tool_ids[tool_id] == 0
        )
        anomalies["tool_uses_without_result"] += sum(
            count for tool_id, count in tool_ids.items() if result_ids[tool_id] == 0
        )

    todo_summary = {
        call_id: {
            "observations": len(rows),
            "positions": sorted({position for _, position, _ in rows}),
            "pair_hashes": sorted({digest for _, _, digest in rows}),
            "first_file": rows[0][0],
            "last_file": rows[-1][0],
        }
        for call_id, rows in sorted(todo_observations.items())
    }
    return {
        "dump_count": len(dumps),
        "sessions": dict(sorted(sessions.items())),
        "project_roots": counter_dict(project_roots),
        "response_statuses": counter_dict(response_statuses),
        "response_usage_bands": counter_dict(response_usage_bands),
        "max_response_usage": max_response_usage,
        "system_shapes": counter_dict(system_shapes),
        "system_compositions": counter_dict(system_compositions),
        "guidance_suffixes": counter_dict(guidance_suffixes),
        "head_shapes_top40": counter_dict(head_shapes, 40),
        "m0_section_orders": counter_dict(m0_section_orders),
        "m0_section_separators": counter_dict(m0_section_separators),
        "m1_section_orders": counter_dict(m1_section_orders),
        "m1_section_separators": counter_dict(m1_section_separators),
        "compartment_heading_shapes": counter_dict(compartment_heading_shapes),
        "assistant_part_orders_top40": counter_dict(assistant_orders, 40),
        "trailing_shapes_top40": counter_dict(trailing_shapes, 40),
        "text_classes": counter_dict(text_classes),
        "tag_classes": counter_dict(tag_classes),
        "tag_placements": counter_dict(tag_placements),
        "tag_prefix_formats": counter_dict(tag_prefix_formats),
        "temporal_classes": counter_dict(temporal_classes),
        "temporal_tag_orders": counter_dict(temporal_tag_orders),
        "reminder_temporal_classes": counter_dict(reminder_temporal_classes),
        "channel1_reminder_shapes": counter_dict(channel1_reminder_shapes),
        "channel1_template_shapes": counter_dict(channel1_template_shapes),
        "channel1_denominators": counter_dict(channel1_denominators),
        "nudge_assembly_shapes": counter_dict(nudge_assembly_shapes),
        "m1_placeholders": counter_dict(m1_placeholders),
        "tool_input_shapes_top40": counter_dict(tool_input_shapes, 40),
        "reduced_envelopes": counter_dict(reduced_envelopes),
        "tool_special_values_top40": counter_dict(tool_special_values, 40),
        "tool_reduction_arc_shapes_top40": counter_dict(tool_reduction_arc_shapes, 40),
        "tool_reduction_vocabulary_top100": counter_dict(tool_reduction_vocabulary, 100),
        "skeleton_recency_top40": counter_dict(skeleton_recency, 40),
        "placeholder_values": counter_dict(placeholder_values),
        "drop_shapes": counter_dict(drop_shapes),
        "compaction_marker_shapes": counter_dict(compaction_marker_shapes),
        "media_shapes": counter_dict(media_shapes),
        "thinking_shapes": counter_dict(thinking_shapes),
        "reasoning_order_shapes_top40": counter_dict(reasoning_order_shapes, 40),
        "newest_assistant_reasoning_presence": counter_dict(
            newest_assistant_reasoning_presence
        ),
        "newest_assistant_reasoning_shapes_top40": counter_dict(
            newest_assistant_reasoning_shapes, 40
        ),
        "cache_placements": counter_dict(cache_placements),
        "block_key_shapes_top40": counter_dict(message_block_key_shapes, 40),
        "synthetic_todo": todo_summary,
        "anomalies": counter_dict(anomalies),
        "evidence": dict(special_evidence),
    }


def main() -> None:
    args = parse_args()
    rust_sessions = set(args.rust_sessions or RUST_SESSIONS)
    dumps = load_dumps(
        choose_paths(args.dump_dir, args.date, args.per_session, args.after, args.before),
        rust_sessions,
    )
    sessions = {dump.session for dump in dumps}
    report = {
        "method": {
            "date": args.date,
            "per_session": args.per_session,
            "after": args.after,
            "before": args.before,
            "rust_sessions": sorted(rust_sessions),
            "context_db": str(args.context_db) if args.context_db else None,
            "store_db": str(args.store_db) if args.store_db else None,
        },
        "lanes": {
            lane: summarize_lane([dump for dump in dumps if dump.lane == lane])
            for lane in ("rust", "ts")
        },
    }
    if args.context_db is not None or args.store_db is not None:
        report["telemetry"] = summarize_telemetry(
            args.context_db,
            args.store_db,
            sessions,
            rust_sessions,
            parse_bound(args.after, args.date),
            parse_bound(args.before, args.date, end=True),
        )
    print(json.dumps(report, ensure_ascii=False, indent=args.indent, sort_keys=True))


if __name__ == "__main__":
    main()
