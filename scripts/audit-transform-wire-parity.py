#!/usr/bin/env python3
"""Summarize structural evidence from anthropic-auth request wire dumps.

The audit intentionally compares serialized request bodies rather than trying to
reconstruct them from transform state. Rust-mode sessions are explicit because
that lane assignment is not encoded in the dump filename.
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable

RUST_SESSIONS = {
    "ses_0ad83017cffexe0g5N8UG0y3LZ",
    "ses_08df2045bffeBcWcqw60elghER",
}
SESSION_PATTERN = re.compile(r"-(ses_[^-]+)-")
TAG_PATTERN = re.compile(r"^§(\d+)§(?: |$)")
TEMPORAL_PATTERN = re.compile(r"^<!-- \+[^>]+ -->\n")
DROP_PATTERN = re.compile(r"^\[dropped(?: §\d+§)?\]$")
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


@dataclass(frozen=True)
class Dump:
    path: Path
    session: str
    lane: str
    body: dict[str, Any]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("dump_dir", type=Path)
    parser.add_argument("--date", default="2026-08-24")
    parser.add_argument("--per-session", type=int, default=6)
    parser.add_argument(
        "--after",
        help="include dump filenames at or after this UTC timestamp prefix",
    )
    parser.add_argument("--indent", type=int, default=2)
    return parser.parse_args()


def session_from_name(path: Path) -> str | None:
    match = SESSION_PATTERN.search(path.name)
    return match.group(1) if match else None


def choose_paths(
    root: Path, date: str, per_session: int, after: str | None = None
) -> list[Path]:
    grouped: dict[str, list[Path]] = collections.defaultdict(list)
    for path in root.glob(f"{date}*.body.json"):
        if after is not None and path.name < after:
            continue
        session = session_from_name(path)
        if session is not None:
            grouped[session].append(path)
    return [
        path
        for session in sorted(grouped)
        for path in sorted(grouped[session])[-per_session:]
    ]


def load_dumps(paths: Iterable[Path]) -> list[Dump]:
    dumps = []
    for path in paths:
        session = session_from_name(path)
        if session is None:
            continue
        dumps.append(
            Dump(
                path=path,
                session=session,
                lane="rust" if session in RUST_SESSIONS else "ts",
                body=json.loads(path.read_text()),
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


def summarize_lane(dumps: list[Dump]) -> dict[str, Any]:
    sessions = collections.Counter(dump.session for dump in dumps)
    system_shapes: collections.Counter[Any] = collections.Counter()
    head_shapes: collections.Counter[Any] = collections.Counter()
    assistant_orders: collections.Counter[Any] = collections.Counter()
    trailing_shapes: collections.Counter[Any] = collections.Counter()
    tag_classes: collections.Counter[Any] = collections.Counter()
    temporal_classes: collections.Counter[Any] = collections.Counter()
    temporal_tag_orders: collections.Counter[Any] = collections.Counter()
    m1_placeholders: collections.Counter[Any] = collections.Counter()
    tool_input_shapes: collections.Counter[Any] = collections.Counter()
    reduced_envelopes: collections.Counter[Any] = collections.Counter()
    tool_special_values: collections.Counter[Any] = collections.Counter()
    placeholder_values: collections.Counter[Any] = collections.Counter()
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
        body = dump.body
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
            for index, item in enumerate(system):
                if isinstance(item, dict) and "cache_control" in item:
                    cache_placements[("system", index, item.get("type"))] += 1
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
                if value == M1_PLACEHOLDER_TEXT:
                    m1_placeholders[("bare", block_index)] += 1
                elif value == M1_PLACEHOLDER_WRAPPED:
                    m1_placeholders[("wrapped", block_index)] += 1
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
                if "cache_control" in block:
                    cache_placements[("message", role, block_type)] += 1
                if block_type == "tool_use":
                    tool_id = block.get("id")
                    if isinstance(tool_id, str):
                        tool_ids[tool_id] += 1
                    tool_input = block.get("input")
                    if isinstance(tool_input, dict):
                        tool_input_shapes[(block.get("name"), tuple(sorted(tool_input)))] += 1
                        if "reduced" in tool_input or "summary" in tool_input:
                            reduced_envelopes[
                                (
                                    block.get("name"),
                                    tuple(sorted(tool_input)),
                                    type(tool_input.get("reduced")).__name__,
                                    type(tool_input.get("summary")).__name__,
                                )
                            ] += 1
                        for path, value in json_paths(tool_input):
                            if isinstance(value, str) and "...[truncated]" in value:
                                key = (block.get("name"), path, "...[truncated]")
                                tool_special_values[key] += 1
                                if len(special_evidence["tool_input"]) < 12:
                                    special_evidence["tool_input"].append(
                                        evidence(dump, message_index, block_index, value)
                                    )
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
                    tag = TAG_PATTERN.match(value)
                    if tag:
                        tag_classes[(role, block_type, field)] += 1
                        if len(special_evidence["tag"]) < 6:
                            special_evidence["tag"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    temporal = TEMPORAL_PATTERN.match(value)
                    if temporal or TAG_TEMPORAL_PATTERN.match(value):
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
                    if DROP_PATTERN.fullmatch(value):
                        placeholder_values[
                            "tagged_dropped" if "§" in value else "bare_dropped"
                        ] += 1
                        if len(special_evidence["drop"]) < 12:
                            special_evidence["drop"].append(
                                evidence(dump, message_index, block_index, value)
                            )
                    if "[Compacted by magic-context" in value:
                        placeholder_values["compaction_summary"] += 1
                    if "...[truncated]" in value:
                        placeholder_values["...[truncated]"] += 1
                    elif "truncated" in value.lower():
                        placeholder_values["other_truncated_text"] += 1

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
        "system_shapes": counter_dict(system_shapes),
        "head_shapes_top40": counter_dict(head_shapes, 40),
        "assistant_part_orders_top40": counter_dict(assistant_orders, 40),
        "trailing_shapes_top40": counter_dict(trailing_shapes, 40),
        "tag_classes": counter_dict(tag_classes),
        "temporal_classes": counter_dict(temporal_classes),
        "temporal_tag_orders": counter_dict(temporal_tag_orders),
        "m1_placeholders": counter_dict(m1_placeholders),
        "tool_input_shapes_top40": counter_dict(tool_input_shapes, 40),
        "reduced_envelopes": counter_dict(reduced_envelopes),
        "tool_special_values_top40": counter_dict(tool_special_values, 40),
        "placeholder_values": counter_dict(placeholder_values),
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
    dumps = load_dumps(
        choose_paths(args.dump_dir, args.date, args.per_session, args.after)
    )
    report = {
        "method": {
            "date": args.date,
            "per_session": args.per_session,
            "after": args.after,
            "rust_sessions": sorted(RUST_SESSIONS),
        },
        "lanes": {
            lane: summarize_lane([dump for dump in dumps if dump.lane == lane])
            for lane in ("rust", "ts")
        },
    }
    print(json.dumps(report, ensure_ascii=False, indent=args.indent, sort_keys=True))


if __name__ == "__main__":
    main()
