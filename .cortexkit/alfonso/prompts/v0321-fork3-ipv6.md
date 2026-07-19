# Fork 3 — deny IPv6 egress for smart-note checks (v0.32.1)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Ruling: DENY IPv6 egress entirely for smart-note compiled-check network access (the reachability given up — IPv6-only external hosts — is negligible, and it eliminates the NAT64/Pref64 bypass class rather than chasing registry drift). Verify at source first.

## The bug (W4-H M6, verified)
packages/plugin/src/features/magic-context/smart-notes/ssrf-guard.ts (~:445-461) excludes only the well-known NAT64 prefixes (64:ff9b::/96, 64:ff9b:1::/48) but NOT network-specific Pref64 (RFC 6052): an internal host whose A record is e.g. 10.0.0.5 can be synthesized into a GLOBAL-looking IPv6 address that passes the guard, and TLS succeeds because the internal service owns the hostname's cert → the guard is bypassed. Also the test wrongly accepts the non-global 3fff::/20 documentation range.

## Fix
In the SSRF guard used by smart-note checks (the DNS-resolving, IP-pinning path): reject ALL IPv6 destination addresses for smart-note egress. Concretely — when resolving/validating candidate addresses, if a resolved address is IPv6 (family 6 / contains ':'), fail closed (SmartNoteNetworkError, same class as the other guard rejections). This means smart-note httpGet only connects over IPv4. Keep IPv4 behavior exactly as-is (loopback/RFC1918/link-local/metadata blocks unchanged). Do NOT change the dashboard's separate SSRF guard or the embedding SSRF guard — this is the smart-note check path only.
Also fix the test that accepts 3fff::/20 — it should now be rejected (as should every IPv6), so update/replace that assertion.

## Tests
- a hostname resolving only to an IPv6 address → rejected (SmartNoteNetworkError).
- a NAT64/Pref64-synthesized global-looking IPv6 embedding an internal IPv4 → rejected (covered by the blanket IPv6 deny).
- 3fff::/20 → rejected.
- an IPv4 public host → still allowed (unchanged).
- an IPv4 loopback/RFC1918 → still rejected (unchanged).

## Gates
packages/plugin: bun test src/features/magic-context/smart-notes, typecheck, lint, check_comments. Comment explains WHY IPv6 is denied wholesale (NAT64/Pref64 synthesis defeats prefix-classification; negligible reachability cost). Report status + test evidence.
