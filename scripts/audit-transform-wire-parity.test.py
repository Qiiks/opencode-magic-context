#!/usr/bin/env python3
"""Hermetic machinery-audit smoke test with served bytes and durable rows."""

from __future__ import annotations

import datetime as dt
import json
import sqlite3
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "audit-transform-wire-parity.py"
DATE = "2026-08-27"


class AuditTransformWireParityTest(unittest.TestCase):
    def test_config_verified_lanes_facades_and_historian_rows(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            temp = Path(temporary)
            dump_dir = temp / "dumps"
            dump_dir.mkdir()
            rust_root = temp / "rust-project"
            ts_root = temp / "ts-project"
            for project in (rust_root, ts_root):
                (project / ".cortexkit").mkdir(parents=True)
            (rust_root / ".cortexkit" / "magic-context.jsonc").write_text(
                '{\n  // Live lane authority.\n  "transform_mode": "rust",\n}\n'
            )
            (ts_root / ".cortexkit" / "magic-context.jsonc").write_text(
                "{ // Empty means the TypeScript default.\n}\n"
            )
            self._write_dump(dump_dir, "ses_rust", rust_root, "rust-call")
            self._write_dump(dump_dir, "ses_ts", ts_root, "ts-call")
            pi_session_dir = temp / "pi-sessions"
            pi_render_dir = temp / "pi-renders"
            pi_session_dir.mkdir()
            pi_render_dir.mkdir()
            self._write_pi_session(pi_session_dir)
            self._write_pi_render(pi_render_dir, ts_root)

            context_db = temp / "context.db"
            store_db = temp / "store.db"
            self._write_context_db(context_db)
            self._write_store_db(store_db)

            completed = subprocess.run(
                [
                    "python3",
                    str(SCRIPT),
                    str(dump_dir),
                    "--date",
                    DATE,
                    "--per-session",
                    "10",
                    "--context-db",
                    str(context_db),
                    "--store-db",
                    str(store_db),
                    "--pi-session-dir",
                    str(pi_session_dir),
                    "--pi-render-dir",
                    str(pi_render_dir),
                ],
                cwd=ROOT,
                check=True,
                capture_output=True,
                text=True,
            )
            report = json.loads(completed.stdout)
            self.assertEqual(
                report["lane_verification"]["denominator_dump_counts"],
                {"rust": 1, "ts": 1},
            )
            rows = report["lane_verification"]["sessions"]
            rust = next(row for row in rows if row["session"] == "ses_rust")
            ts = next(row for row in rows if row["session"] == "ses_ts")
            self.assertEqual(rust["configured_lane"], "rust")
            self.assertEqual(rust["status"], "label_corrected_from_live_config")
            self.assertEqual(ts["configured_lane"], "ts")
            self.assertEqual(report["excluded_unverified_dumps"], [])
            self.assertEqual(
                report["pi_lane_verification"]["denominator_dump_counts"], {"pi": 1}
            )
            self.assertEqual(report["lanes"]["pi"]["dump_count"], 1)
            self.assertEqual(report["pi_session_sources"]["totals"]["files"], 1)
            self.assertEqual(
                report["pi_session_sources"]["totals"]["missing_entry_ids"], 0
            )
            tagging = next(
                axis
                for axis in report["ts_pi_cross_harness_parity"]["axes"]
                if axis["axis"] == "tagging_and_fallback_adoption"
            )
            self.assertEqual(tagging["verdict"], "matched_shape_space")
            self.assertEqual(
                report["ts_pi_cross_harness_parity"]["unexplained_byte_classes"], []
            )

            facades = report["ctx_facade_parity"]
            self.assertEqual(len(facades["matched_input_classes"]), 1)
            self.assertEqual(facades["matched_input_classes"][0]["verdict"], "byte_equal")
            self.assertEqual(facades["unexplained_byte_classes"], [])

            telemetry = report["telemetry"]
            rust_rows = telemetry["rust_historian_rows"]
            ts_rows = telemetry["ts_historian_rows"]
            self.assertEqual(rust_rows["compartments"]["rows_born_in_window"], 1)
            self.assertEqual(rust_rows["compartments"]["complete_date_rows"], 1)
            self.assertEqual(ts_rows["compartments"]["rows_born_in_window"], 1)
            self.assertIsNone(ts_rows["compartments"]["complete_date_rows"])
            self.assertEqual(rust_rows["promoted_facts"]["rows_promoted_in_window"], 1)
            self.assertEqual(ts_rows["promoted_facts"]["rows_promoted_in_window"], 1)
            self.assertIn("mc_historian_side_channel_outbox", rust_rows["session_id_tables"])
            self.assertIn("compartment_events", ts_rows["session_id_tables"])

            adjacent = report["engine_adjacent_state"]
            self.assertEqual(adjacent["unexplained_invariants"], [])
            self.assertTrue(
                adjacent["coverage_by_lane"]["rust"][0]["message_index_present"]
            )
            self.assertTrue(
                adjacent["coverage_by_lane"]["ts"][0]["message_index_present"]
            )
            self.assertTrue(
                adjacent["coverage_by_lane"]["rust"][0]["chunk_vectors_present"]
            )
            self.assertTrue(
                adjacent["coverage_by_lane"]["ts"][0]["memory_vectors_present"]
            )
            self.assertEqual(
                adjacent["per_session"]["ses_rust"]["rust_engine_truth"][
                    "mc_cache_state"
                ][0]["last_activity_at"],
                int(dt.datetime(2026, 8, 27, 12, tzinfo=dt.timezone.utc).timestamp() * 1000),
            )
            self.assertEqual(
                adjacent["per_session"]["ses_rust"]["rust_engine_truth"][
                    "mc_pass_trace"
                ][0]["receive_count"],
                3,
            )
            self.assertEqual(
                adjacent["per_session"]["ses_ts"]["rust_engine_truth"][
                    "mc_cache_state"
                ],
                [],
            )

    def _write_dump(
        self, dump_dir: Path, session: str, project: Path, call_id: str
    ) -> None:
        name = f"{DATE}T12-00-00-000Z-000001-{session}-direct.body.json"
        body = {
            "system": [{"type": "text", "text": f"Working directory: {project}"}],
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": call_id,
                            "name": "ctx_expand",
                            "input": {"message": 7},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": "§42§ [7] A (assistant) — full recovery:\n\n  [text]\nhello",
                        }
                    ],
                },
            ],
        }
        path = dump_dir / name
        path.write_text(json.dumps(body))
        path.with_name(name.replace(".body.json", ".response.json")).write_text(
            json.dumps({"status": 200, "usage": {"input_tokens": 100}})
        )

    def _write_pi_session(self, directory: Path) -> None:
        entries = [
            {
                "type": "session",
                "id": "pi_session",
                "cwd": "/fixture/project",
                "timestamp": "2026-08-27T12:00:00Z",
            },
            {
                "type": "message",
                "id": "pi-user-1",
                "message": {"role": "user", "content": "§1§ hello", "timestamp": 1},
            },
            {
                "type": "message",
                "id": "pi-tool-1",
                "message": {
                    "role": "toolResult",
                    "toolCallId": "pi-call",
                    "toolName": "ctx_expand",
                    "content": [{"type": "text", "text": "§42§ recovered"}],
                    "timestamp": 2,
                },
            },
        ]
        (directory / "pi-session.jsonl").write_text(
            "".join(json.dumps(entry) + "\n" for entry in entries)
        )

    def _write_pi_render(self, directory: Path, project: Path) -> None:
        capture = {
            "session_id": "pi_session",
            "project_root": str(project),
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "toolCall",
                            "id": "pi-call",
                            "name": "ctx_expand",
                            "arguments": {"message": 7},
                        }
                    ],
                },
                {
                    "role": "toolResult",
                    "toolCallId": "pi-call",
                    "toolName": "ctx_expand",
                    "content": [
                        {
                            "type": "text",
                            "text": "§42§ [7] A (assistant) — full recovery:\n\n  [text]\nhello",
                        }
                    ],
                },
            ],
        }
        (directory / f"{DATE}T12-00-00.pi-render.json").write_text(json.dumps(capture))

    def _write_context_db(self, path: Path) -> None:
        with sqlite3.connect(path) as db:
            db.executescript(
                """
                CREATE TABLE transform_decisions (
                    session_id TEXT, ts_ms INTEGER, decision TEXT, materialize_reason TEXT,
                    input_tokens INTEGER, emergency INTEGER, dropped_count INTEGER,
                    system_hash_prev TEXT, system_hash_new TEXT, m0_model_key_prev TEXT,
                    m0_model_key_new TEXT, m0_tool_set_hash_prev TEXT, m0_tool_set_hash_new TEXT
                );
                CREATE TABLE tags (session_id TEXT, caveman_depth INTEGER, tag_number INTEGER);
                CREATE TABLE compartments (
                    id INTEGER PRIMARY KEY, session_id TEXT, harness TEXT, sequence INTEGER,
                    start_message INTEGER, end_message INTEGER, p1 TEXT, p2 TEXT, p3 TEXT,
                    p4 TEXT, importance INTEGER, legacy INTEGER, created_at INTEGER
                );
                CREATE TABLE memories (
                    id INTEGER PRIMARY KEY, project_path TEXT, source_session_id TEXT,
                    category TEXT, content TEXT, normalized_hash TEXT, importance INTEGER,
                    source_type TEXT, created_at INTEGER
                );
                CREATE TABLE message_history_index (
                    session_id TEXT PRIMARY KEY, last_indexed_ordinal INTEGER,
                    dirty_floor_ordinal INTEGER, harness TEXT
                );
                CREATE TABLE message_history_fts (
                    session_id TEXT, message_ordinal INTEGER, message_id TEXT, role TEXT,
                    content TEXT
                );
                CREATE TABLE session_projects (
                    session_id TEXT, harness TEXT, project_path TEXT
                );
                CREATE TABLE compartment_chunk_embeddings (
                    compartment_id INTEGER, session_id TEXT, project_path TEXT, harness TEXT
                );
                CREATE TABLE memory_embeddings (memory_id INTEGER, model_id TEXT);
                CREATE TABLE notes (
                    session_id TEXT, type TEXT, status TEXT, check_status TEXT
                );
                CREATE TABLE git_commits (project_path TEXT, sha TEXT);
                CREATE TABLE compartment_events (
                    session_id TEXT, kind TEXT, at_compartment INTEGER, created_at INTEGER
                );
                """
            )
            now = int(
                dt.datetime(2026, 8, 27, 12, tzinfo=dt.timezone.utc).timestamp() * 1000
            )
            db.execute(
                "INSERT INTO transform_decisions VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ("ses_ts", now, "defer", None, 100, 0, 0, "a", "a", "m", "m", "t", "t"),
            )
            db.execute(
                "INSERT INTO compartments VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (1, "ses_ts", "opencode", 1, 1, 4, "p1", "p2", "p3", "p4", 61, 0, now),
            )
            db.execute(
                "INSERT INTO memories VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (1, "git:ts", "ses_ts", "workflow", "ts fact", "ts-hash", 50, "historian", now),
            )
            db.executemany(
                "INSERT INTO message_history_index VALUES (?, 1, NULL, 'opencode')",
                [("ses_ts",), ("ses_rust",)],
            )
            db.executemany(
                "INSERT INTO message_history_fts VALUES (?, 1, ?, 'user', ?)",
                [
                    ("ses_ts", "ts-message", "identical searchable bytes"),
                    ("ses_rust", "rust-message", "identical searchable bytes"),
                ],
            )
            db.executemany(
                "INSERT INTO session_projects VALUES (?, 'opencode', ?)",
                [("ses_ts", "git:ts"), ("ses_rust", "git:rust")],
            )
            db.executemany(
                "INSERT INTO compartment_chunk_embeddings VALUES (?, ?, ?, 'opencode')",
                [(1, "ses_ts", "git:ts"), (2, "ses_rust", "git:rust")],
            )
            db.execute("INSERT INTO memory_embeddings VALUES (1, 'fixture-model')")
            db.executemany(
                "INSERT INTO notes VALUES (?, 'smart', 'active', 'compiled')",
                [("ses_ts",), ("ses_rust",)],
            )
            db.executemany(
                "INSERT INTO git_commits VALUES (?, ?)",
                [("git:ts", "abcdef1"), ("git:rust", "abcdef2")],
            )
            db.execute(
                "INSERT INTO compartment_events VALUES (?, ?, ?, ?)",
                ("ses_ts", "decision", 1, now),
            )

    def _write_store_db(self, path: Path) -> None:
        with sqlite3.connect(path) as db:
            db.executescript(
                """
                CREATE TABLE mc_cache_state (
                    session_id TEXT, last_activity_at INTEGER, meta TEXT
                );
                CREATE TABLE mc_pass_trace (
                    session_id TEXT, scheduler_history TEXT,
                    scheduler_interesting_history TEXT, last_received_at_ms INTEGER,
                    last_completed_at_ms INTEGER, last_reject_error TEXT,
                    last_reject_at_ms INTEGER, receive_count INTEGER, reject_count INTEGER,
                    first_divergence TEXT
                );
                CREATE TABLE mc_compartments (
                    session_id TEXT, sequence INTEGER, start_message INTEGER, end_message INTEGER,
                    start_date TEXT, end_date TEXT, p1 TEXT, p2 TEXT, p3 TEXT, p4 TEXT,
                    importance INTEGER, legacy INTEGER, created_at INTEGER
                );
                CREATE TABLE mc_memories (
                    source_session_id TEXT, category TEXT, content TEXT, importance INTEGER,
                    source_type TEXT, created_at INTEGER
                );
                CREATE TABLE mc_historian_side_channel_outbox (
                    session_id TEXT, kind TEXT, firing_seq INTEGER, source_start INTEGER,
                    source_end INTEGER, attempt_count INTEGER, delivered_at_ms INTEGER,
                    last_error TEXT, created_at_ms INTEGER
                );
                """
            )
            now = int(
                dt.datetime(2026, 8, 27, 12, tzinfo=dt.timezone.utc).timestamp() * 1000
            )
            db.execute(
                "INSERT INTO mc_cache_state VALUES (?, ?, ?)",
                ("ses_rust", now, json.dumps({"caveman_age_basis_tag": 9})),
            )
            db.execute(
                "INSERT INTO mc_pass_trace VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ("ses_rust", "[]", "[]", now, now, None, None, 3, 0, None),
            )
            db.execute(
                "INSERT INTO mc_compartments VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "ses_rust",
                    1,
                    1,
                    4,
                    "2026-08-27",
                    "2026-08-27",
                    "p1",
                    "p2",
                    "p3",
                    "p4",
                    63,
                    0,
                    now,
                ),
            )
            db.execute(
                "INSERT INTO mc_memories VALUES (?, ?, ?, ?, ?, ?)",
                ("ses_rust", "workflow", "rust fact", 50, "historian", now),
            )
            db.execute(
                "INSERT INTO mc_historian_side_channel_outbox VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ("ses_rust", "event", 1, 1, 4, 0, now, None, now),
            )


if __name__ == "__main__":
    unittest.main()
