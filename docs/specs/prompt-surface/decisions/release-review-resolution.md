# v0.35.0 release review resolution (consult ct_00000000-0000-4007-98cb-a21c64f40248)

Verdict was SHIP on Q2 (flip safety), Q3 (full-hash back-compat), Q4
(Pi parity), with two BLOCK legs. Both resolved with direct evidence:

## BLOCK 1 — A1 golden provenance: RESOLVED BY MEASUREMENT
Panel: "no evidence the golden predates the wave commits."
Resolution: the golden file was added in the S1 commit (b43ccd53), and
rendering the guidance at the pre-wave revision (68d42379 = b43ccd53^)
reproduces the golden's pinned MD5 exactly:
    pre-wave buildMagicContextSection md5 = e8606e8e36cf19f5588ae0a76ce23b0e
    golden PRIMARY full md5              = e8606e8e36cf19f5588ae0a76ce23b0e
The baseline is independently proven pre-wave, not documentation-trusted.

## Finding — resolveCacheTtl walk widened: CONFIRMED REAL, SHIPPED AS
## DOCUMENTED BEHAVIOR IMPROVEMENT
Panel suspected provider/* wildcard support might be new to cache_ttl.
Confirmed at the pre-wave source (event-resolvers.ts): the old walk was
exact -> bare -> default. The unified walk adds progressive
dash-stripping and provider/* wildcards to cache_ttl. Consequence: a
config carrying a previously-inert "provider/*" cache_ttl key becomes
live on upgrade. Decision: keep the unification (one walk for one
config file is the structural point of S1; freezing legacy semantics
would mean two walks forever) and document the change in the v0.35.0
release notes. The unified walk is pinned by the shared parity test.

## BLOCK 2 — mixed-version plugin/module matrix: RESOLVED BY
## DEPLOYMENT TOPOLOGY + ANCESTRY
Panel: "bidirectional deployment coherence not proven."
Resolution: the module surfaces (rust transform mode, CC-leg manifest)
are not public-release surfaces — transform_mode:"rust" is an
undocumented dev-only flag and the CC leg is our own managed
deployment. The one real deployment (prod ck-mc) carries S5:
git merge-base --is-ancestor 5cf67037 cc842547 = true, and the running
binary's placement was ladder-verified at deploy. For hypothetical
skew the structural argument stands (old module still HARD-folds on
the salted hash; light falls back to full with an explicit notice —
degrades safe, never silent). npm users have no module in the path.
