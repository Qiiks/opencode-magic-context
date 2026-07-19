# Build a blind P5 cue-line quiz from real memory data

You are building materials for a blind recall experiment. IMPORTANT: your final report must NOT contain any memory content or cue text — the quiz subject reads your report. Report only file paths and counts.

## Task
1. Open ~/.local/share/cortexkit/magic-context/context.db READ-ONLY (bun:sqlite, { readonly: true }). Do not write to this DB.
2. Inspect the `memories` table schema (PRAGMA table_info). Select 30 memories:
   - status = 'active'
   - belonging to the magic-context project (inspect the project identity/path column; pick the identity that matches paths containing 'magic-context')
   - content length >= 100 chars
   - LOW importance (bottom half of the importance distribution among that project's active memories; if importance is NULL for many, prefer NULLs and low values)
   - spread across categories (aim for a mix of PROJECT_RULES / ARCHITECTURE / CONSTRAINTS / CONFIG_VALUES, not all one category)
   - randomize selection within those constraints (ORDER BY RANDOM())
3. Randomly split them 15/15 into condition P (pidgin cue) and condition T (truncation).
4. For each condition-P memory, author a P5 PIDGIN CUE LINE, budget ~8-12 tokens:
   - Grammar: `#<id> <domain glyph(s)> <2-4 discriminating anchors> <optional outcome glyph>`
   - Glyph vocabulary: emoji (💾🐛🔒🔌📦🕸⏱🧊✂🔀♻🧪) and single CJK characters (記 memory, 影 shadow, 修 repair, 漏 leak, 速 speed, 鎖 lock, 約 contract) where they carry a concept in one glyph.
   - Anchors must be DISCRIMINATING strings from the memory: identifiers, file names, config keys, version numbers, error codes, table/column names, exact function names. 
   - BAN generic words: error, bug, fix, issue, problem, config, database (as bare words), important, must.
   - The cue should evoke the memory for someone who once knew it, without stating the rule. Do NOT write a sentence. Do NOT paraphrase the whole memory.
5. For each condition-T memory, produce the mechanical control: `#<id> ` + the first 40 characters of the memory content, verbatim, then "…".
6. Write files:
   - /tmp/p5-quiz/quiz.md — all 30 lines, SHUFFLED order, numbered Q1..Q30. Each line: `Q<n>: <cue or truncation line>`. Do not label which condition each line is.
   - /tmp/p5-quiz/answer-key.md — for each Q number: the memory id, condition (P or T), category, importance, and the FULL memory content.
   - /tmp/p5-quiz/stats.md — counts per condition and category, and the token estimate (chars/3.5) of the full quiz file vs the summed full-content size (the compression ratio).
7. FINAL REPORT: only the three file paths, the selection counts, and the compression ratio. NO memory content, NO cue text, NO ids.

No repo changes, no commit needed. This is a data-preparation task only.