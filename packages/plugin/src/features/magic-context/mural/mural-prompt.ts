export interface MuralSourceMemory {
    id: number;
    category: string;
    importance: number;
    content: string;
}

export interface MuralManifestEntry {
    id: number;
    category: string;
    room: string;
    importance: number;
    cue?: string;
    mergeInto?: number;
}

/** The production authoring contract. It is deliberately data-free: source
 * memories are appended by the task runner, never embedded in this module. */
export const MURAL_AUTHORING_PROMPT = `You author a selective memory mural from the supplied overflow memory pool. The pool contains memories that did not fit the m0 budget. Select only the memories that matter most, compress each cue hard, and rank rooms and cues by importance. The renderer fills one fixed image top-down and drops the tail, so rank the highest-value material first.

Your reply must begin with <mural and end with </mural>. Return exactly one XML manifest and nothing else.

Required shape:
<mural category="PROJECT_RULES">
  <room name="Short concrete hub">
    <entry id="7863" importance="82">compressed cue</entry>
    <merge id="8255" into="7863"/>
  </room>
</mural>

Rules:
- Copy each selected source id and importance exactly. Omit lower-value memories when useful; never duplicate an id.
- A merge has no cue and targets a non-merged entry in the same room and category.
- Preserve exact identifiers, paths, commands, flags, versions, filenames, hashes, and code tokens verbatim.
- Cues are mnemonic anchors, not prose. Prefer one to three distinctive tokens plus a relation. Use → ← ⊘ ∵ ≺ ≻ ∅ ∀ when shorter.
- Per-cue hard budgets are 90 characters for importance >= 70 and 50 otherwise. Exceeding either budget rejects the manifest.
- Never put a source memory id such as #7863 in a cue. XML-escape &, <, >, and quotes in cue text.
- A prohibition must mark the excluded thing as ⊘thing followed immediately by a terse parenthesized mechanism, such as ⊘cache write (ABI break). Positive facts must be rephrased to avoid trigger words.
- Keep room names compact concrete nouns and do not repeat their hub words in cues.
- Emit the complete root, with no Markdown fence, preamble, explanation, or trailing text.

Synthetic examples (do not reuse their facts):
<mural category="NAMING">
  <room name="Queue worker">
    <entry id="3315" importance="65">mailer id; ⊘mail-worker (lookup mismatch)</entry>
    <merge id="6407" into="3315"/>
  </room>
</mural>

Before emitting, check duplicate ids, same-room merge targets, exact anchors, cue budgets, room/entry ordering, balanced parentheses, and polarity mechanisms.`;

export const MURAL_PROMPT = MURAL_AUTHORING_PROMPT;

export interface MuralCueViolation {
    id: number;
    length: number;
    budget: number;
}

export function muralCueViolations(entries: readonly MuralManifestEntry[]): MuralCueViolation[] {
    return entries.flatMap((entry) => {
        if (entry.cue === undefined) return [];
        const budget = entry.importance >= 70 ? 90 : 50;
        const length = [...entry.cue].length;
        return length > budget ? [{ id: entry.id, length, budget }] : [];
    });
}

function cueHasUnbalancedParentheses(cue: string): boolean {
    let depth = 0;
    for (const character of cue) {
        if (character === "(") depth++;
        if (character === ")") depth--;
        if (depth < 0) return true;
    }
    return depth !== 0;
}

export function validateMuralManifest(
    source: readonly MuralSourceMemory[],
    entries: readonly MuralManifestEntry[],
): MuralCueViolation[] {
    const sourceById = new Map(source.map((memory) => [memory.id, memory]));
    const seen = new Set<number>();
    const violations = muralCueViolations(entries);
    for (const entry of entries) {
        if (seen.has(entry.id)) throw new Error(`duplicate mural memory id ${entry.id}`);
        seen.add(entry.id);
        const original = sourceById.get(entry.id);
        if (!original) throw new Error(`mural memory id ${entry.id} is absent from source`);
        // Category and importance are source facts the model merely copies, and
        // large authors flub the copy often enough to burn retries on it (live
        // first-render lost an attempt to one mis-copied category out of 379
        // entries). Heal them from source instead of rejecting: the model's
        // decisions are selection, room, cue, and rank — never these columns.
        entry.category = original.category;
        if (entry.mergeInto === undefined) entry.importance = original.importance;
        if (entry.mergeInto !== undefined && entry.cue !== undefined)
            throw new Error(`merged mural entry ${entry.id} has a cue`);
        if (entry.cue === undefined) continue;
        if (/#\d+/.test(entry.cue)) throw new Error(`mural memory id leaked into cue ${entry.id}`);
        if (cueHasUnbalancedParentheses(entry.cue))
            throw new Error(`unbalanced mural cue ${entry.id}`);
        const trigger = /\b(?:must not|never|without|instead of|exclude|excludes)\b/i.test(
            entry.cue,
        );
        const markers = entry.cue.split("⊘").length - 1;
        const mechanisms = entry.cue.match(/\([^()]+\)/g)?.length ?? 0;
        if (trigger && markers === 0)
            throw new Error(`mural prohibition missing polarity marker ${entry.id}`);
        if (markers > mechanisms) throw new Error(`mural polarity mechanism missing ${entry.id}`);
    }
    const byId = new Map(entries.map((entry) => [entry.id, entry]));
    for (const entry of entries) {
        if (entry.mergeInto === undefined) continue;
        const target = byId.get(entry.mergeInto);
        if (
            !target ||
            target.mergeInto !== undefined ||
            target.category !== entry.category ||
            target.room !== entry.room
        ) {
            throw new Error(`invalid mural merge target ${entry.mergeInto} for ${entry.id}`);
        }
    }
    if (violations.length > 0) {
        throw new Error(`mural cue budget violations ids ${violations.map((v) => v.id).join(",")}`);
    }
    return violations;
}

export function parseMuralManifest(raw: string): MuralManifestEntry[] {
    const root = raw.trim();
    const rootMatch = root.match(/^<mural\s+category="([^"]+)">([\s\S]*)<\/mural>$/);
    if (!rootMatch) throw new Error("mural response must contain one <mural> root");
    const category = rootMatch[1]!;
    const body = rootMatch[2]!;
    const entries: MuralManifestEntry[] = [];
    const roomPattern = /<room\s+name="([^"]+)">([\s\S]*?)<\/room>/g;
    for (const roomMatch of body.matchAll(roomPattern)) {
        const room = roomMatch[1]!;
        const roomBody = roomMatch[2]!;
        for (const match of roomBody.matchAll(
            /<entry\s+id="(\d+)"\s+importance="(\d+)">([\s\S]*?)<\/entry>|<merge\s+id="(\d+)"\s+into="(\d+)"\s*\/>/g,
        )) {
            const id = Number(match[1] ?? match[4]);
            const importance = match[1] ? Number(match[2]) : 0;
            const cue = match[1]
                ? (match[3] ?? "")
                      .replace(/&lt;/g, "<")
                      .replace(/&gt;/g, ">")
                      .replace(/&amp;/g, "&")
                      .replace(/&quot;/g, '"')
                : undefined;
            entries.push({
                id,
                category,
                room,
                importance,
                ...(cue === undefined ? { mergeInto: Number(match[5]) } : { cue }),
            });
        }
    }
    if (entries.length === 0) throw new Error("mural manifest selected no memories");
    return entries;
}

/** Last-resort manifest salvage: keep every entry that individually passes the
 * cue rules and drop the rest, instead of letting one stubborn cue among
 * hundreds kill the whole weekly render. Category/importance are healed from
 * source (they are copies, not decisions); merges pointing at dropped or
 * invalid targets are dropped with their movers. Returns the surviving entries
 * plus the dropped ids for the task log. */
export function salvageMuralManifest(
    source: readonly MuralSourceMemory[],
    entries: readonly MuralManifestEntry[],
): { entries: MuralManifestEntry[]; droppedIds: number[] } {
    const sourceById = new Map(source.map((memory) => [memory.id, memory]));
    const seen = new Set<number>();
    const dropped: number[] = [];
    const healed: MuralManifestEntry[] = [];
    for (const entry of entries) {
        const original = sourceById.get(entry.id);
        if (seen.has(entry.id) || !original) {
            dropped.push(entry.id);
            continue;
        }
        seen.add(entry.id);
        entry.category = original.category;
        if (entry.mergeInto === undefined) entry.importance = original.importance;
        if (entry.cue !== undefined) {
            const budget = entry.importance >= 70 ? 90 : 50;
            const markers = entry.cue.split("⊘").length - 1;
            const mechanisms = entry.cue.match(/\([^()]+\)/g)?.length ?? 0;
            const trigger = /\b(?:must not|never|without|instead of|exclude|excludes)\b/i.test(
                entry.cue,
            );
            const invalid =
                [...entry.cue].length > budget ||
                /#\d+/.test(entry.cue) ||
                cueHasUnbalancedParentheses(entry.cue) ||
                (trigger && markers === 0) ||
                markers > mechanisms ||
                (entry.mergeInto !== undefined && entry.cue !== undefined);
            if (invalid) {
                dropped.push(entry.id);
                seen.delete(entry.id);
                continue;
            }
        }
        healed.push(entry);
    }
    const byId = new Map(healed.map((entry) => [entry.id, entry]));
    const entriesOut: MuralManifestEntry[] = [];
    for (const entry of healed) {
        if (entry.mergeInto !== undefined) {
            const target = byId.get(entry.mergeInto);
            if (
                !target ||
                target.mergeInto !== undefined ||
                target.category !== entry.category ||
                target.room !== entry.room
            ) {
                dropped.push(entry.id);
                continue;
            }
        }
        entriesOut.push(entry);
    }
    return { entries: entriesOut, droppedIds: dropped };
}
