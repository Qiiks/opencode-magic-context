#!/usr/bin/env bun
import { Database } from "bun:sqlite";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";

import { getMagicContextStorageDir } from "../src/shared/data-path";

export const RESTORE_REASON =
    "curate user-profile redundancy archive reverted 2026-09-02; no surviving successor";

interface ArchivedMemoryRow {
    id: number;
    project_path: string;
    category: string;
    content: string;
    importance: number;
    metadata_json: string | null;
    superseded_by_memory_id: number | null;
    successor_status: string | null;
}

export interface RestoreReportGroup {
    project: string;
    category: string;
    importanceBand: string;
    count: number;
}

export interface RestoreReport {
    apply: boolean;
    total: number;
    restored: number;
    groups: RestoreReportGroup[];
    highImportanceIds: number[];
    bumpedProjects: string[];
}

const PROFILE_REFERENCE = /\buser(?:[\s_-]+)(?:profile|preferences?)\b/i;
const REDUNDANCY_SHAPE = /\b(?:redundan\w*|duplicat\w*|overlap\w*|subsum\w*|same\s+as)\b/i;
const CANONICAL_PROJECT_CATEGORIES = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
] as const;

function matchesCurateUserProfileArchive(reason: string): boolean {
    return PROFILE_REFERENCE.test(reason) && REDUNDANCY_SHAPE.test(reason);
}

function archiveReason(metadataJson: string | null): string {
    if (!metadataJson) return "";
    try {
        const metadata = JSON.parse(metadataJson) as unknown;
        if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return "";
        const reason = (metadata as Record<string, unknown>).archive_reason;
        return typeof reason === "string" ? reason : "";
    } catch {
        return "";
    }
}

function restoredMetadata(metadataJson: string | null): string {
    let metadata: Record<string, unknown> = {};
    if (metadataJson) {
        try {
            const parsed = JSON.parse(metadataJson) as unknown;
            if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
                metadata = { ...(parsed as Record<string, unknown>) };
            }
        } catch {
            metadata = {};
        }
    }
    delete metadata.archive_reason;
    metadata.restore_reason = RESTORE_REASON;
    return JSON.stringify(metadata);
}

function importanceBand(importance: number): string {
    if (importance >= 85) return "85-100";
    if (importance >= 70) return "70-84";
    if (importance >= 50) return "50-69";
    return "0-49";
}

function loadCandidates(db: Database): ArchivedMemoryRow[] {
    const rows = db
        .prepare(
            `SELECT m.id,
                    m.project_path,
                    m.category,
                    m.content,
                    COALESCE(m.importance, 50) AS importance,
                    m.metadata_json,
                    m.superseded_by_memory_id,
                    successor.status AS successor_status
               FROM memories AS m
               LEFT JOIN memories AS successor
                 ON successor.id = m.superseded_by_memory_id
               WHERE m.status = 'archived'
                 AND m.category IN (?, ?, ?, ?, ?)
               ORDER BY m.project_path, m.category, m.id`,
        )
        .all(...CANONICAL_PROJECT_CATEGORIES) as ArchivedMemoryRow[];
    return rows.filter(
        (row) =>
            row.successor_status !== "active" &&
            matchesCurateUserProfileArchive(archiveReason(row.metadata_json)),
    );
}

function summarize(rows: ArchivedMemoryRow[], apply: boolean, bumpedProjects: string[]): RestoreReport {
    const grouped = new Map<string, RestoreReportGroup>();
    for (const row of rows) {
        const band = importanceBand(row.importance);
        const key = JSON.stringify([row.project_path, row.category, band]);
        const existing = grouped.get(key);
        if (existing) existing.count += 1;
        else {
            grouped.set(key, {
                project: row.project_path,
                category: row.category,
                importanceBand: band,
                count: 1,
            });
        }
    }
    return {
        apply,
        total: rows.length,
        restored: apply ? rows.length : 0,
        groups: [...grouped.values()].sort(
            (left, right) =>
                left.project.localeCompare(right.project) ||
                left.category.localeCompare(right.category) ||
                left.importanceBand.localeCompare(right.importanceBand),
        ),
        highImportanceIds: rows
            .filter((row) => row.importance >= 70)
            .map((row) => row.id)
            .sort((left, right) => left - right),
        bumpedProjects: [...bumpedProjects].sort(),
    };
}

export function restoreCurateArchives(
    db: Database,
    options: { apply: boolean; now?: number },
): RestoreReport {
    if (!options.apply) return summarize(loadCandidates(db), false, []);

    const now = options.now ?? Date.now();
    const appliedRows = db.transaction(() => {
        const candidates = loadCandidates(db);
        const restored: ArchivedMemoryRow[] = [];
        const projects = new Set<string>();
        const update = db.prepare(
            `UPDATE memories
                SET status = 'active',
                    superseded_by_memory_id = NULL,
                    metadata_json = ?,
                    updated_at = ?
              WHERE id = ? AND status = 'archived'`,
        );
        const logMutation = db.prepare(
            `INSERT INTO memory_mutation_log
                (project_path, mutation_type, target_memory_id, superseded_by_id, category, new_content, queued_at)
             VALUES (?, 'update', ?, NULL, ?, ?, ?)`,
        );
        for (const row of candidates) {
            const result = update.run(restoredMetadata(row.metadata_json), now, row.id);
            // bun:sqlite reports trigger-fired writes in `changes` (a one-row memories
            // UPDATE reads 5 on the production schema: FTS delete+insert plus the
            // authority mirror triggers), so equality against 1 silently skips every
            // row after its UPDATE has already committed. Any positive count means
            // the guarded UPDATE matched.
            if (Number(result.changes) < 1) continue;
            logMutation.run(row.project_path, row.id, row.category, row.content, now);
            restored.push(row);
            projects.add(row.project_path);
        }
        const bumpEpoch = db.prepare(
            `INSERT INTO project_state
                (project_path, project_memory_epoch, project_user_profile_version, updated_at)
             VALUES (?, 1, 0, ?)
             ON CONFLICT(project_path) DO UPDATE SET
                project_memory_epoch = project_memory_epoch + 1,
                updated_at = excluded.updated_at`,
        );
        for (const project of projects) bumpEpoch.run(project, now);
        return { restored, projects: [...projects] };
    })();

    return summarize(appliedRows.restored, true, appliedRows.projects);
}

export function formatRestoreReport(report: RestoreReport): string {
    const lines = [
        `mode: ${report.apply ? "apply" : "dry-run"}`,
        "project | category | importance band | count",
        "--- | --- | --- | ---:",
    ];
    if (report.groups.length === 0) lines.push("(none) | (none) | (none) | 0");
    else {
        for (const group of report.groups) {
            lines.push(
                `${group.project} | ${group.category} | ${group.importanceBand} | ${group.count}`,
            );
        }
    }
    lines.push(`total: ${report.total}`);
    lines.push(
        `importance>=70 ids: ${report.highImportanceIds.length > 0 ? report.highImportanceIds.join(", ") : "(none)"}`,
    );
    if (report.apply) {
        lines.push(`restored: ${report.restored}`);
        lines.push(
            `project epochs bumped: ${report.bumpedProjects.length > 0 ? report.bumpedProjects.join(", ") : "(none)"}`,
        );
    } else {
        lines.push("no writes performed; rerun with --apply to restore these rows");
    }
    return `${lines.join("\n")}\n`;
}

function parseArgs(argv: string[]): { apply: boolean; dbPath: string } {
    let apply = false;
    let explicitDryRun = false;
    let dbPath: string | undefined;
    for (let index = 0; index < argv.length; index += 1) {
        const arg = argv[index];
        if (arg === "--apply") {
            apply = true;
            continue;
        }
        if (arg === "--dry-run") {
            explicitDryRun = true;
            continue;
        }
        if (arg === "--db") {
            const value = argv[index + 1];
            if (!value || value.startsWith("--")) throw new Error("--db requires a path");
            dbPath = resolve(value);
            index += 1;
            continue;
        }
        throw new Error(`Unknown argument: ${arg}`);
    }
    if (apply && explicitDryRun) throw new Error("Choose either --dry-run or --apply, not both");
    return {
        apply,
        dbPath: dbPath ?? join(getMagicContextStorageDir(), "context.db"),
    };
}

if (import.meta.main) {
    try {
        const args = parseArgs(process.argv.slice(2));
        if (!existsSync(args.dbPath)) throw new Error(`database not found: ${args.dbPath}`);
        const db = new Database(args.dbPath, args.apply ? undefined : { readonly: true });
        try {
            process.stdout.write(formatRestoreReport(restoreCurateArchives(db, args)));
        } finally {
            db.close();
        }
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        console.error(`restore-curate-archives: ${message}`);
        process.exitCode = 1;
    }
}
