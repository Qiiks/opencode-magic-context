# Prompt-surface decision records

These records are the ratification gate for S2. A record is valid only after it has a committed artifact digest or revision, an explicit decision, the authorized decision owner, a ratification timestamp, and a scope. The campaign owner designates the same authorized owner for both the budget and checklist records here.

Current records are intentionally `PENDING-RATIFICATION`; S3 light authorship is blocked until Ufuk replaces the pending decision/timestamp with a ratification.

## Required record fields

- `recordId`
- `artifactId`
- `artifactRevisionOrDigest`
- `decision`
- `authorizedDecisionOwner`
- `ratificationTimestamp`
- `scope`
- `status`
- `evidence`

## Review order

1. Commit the budget fixture and budget record.
2. Commit the checklist and checklist record.
3. Ufuk ratifies both records, or records an evidence-backed ceiling revision.
4. Only then may S3 author light prose and replace pending compressed mapping targets with exact light lines.

`decision-template.md` is the reusable form. The two sibling records are the pre-filled forms for this S2 artifact set.
