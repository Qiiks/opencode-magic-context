# Incremental native attachment cache design (2026-08-10)

## Cache shape and frontier

`McHandler` owns a 64 MiB LRU cache per session. A session entry is fenced by the durable `revert_epoch` and stores:

- the last acknowledged full-array fingerprint;
- an `Arc<DecodeSidecar>` whose unchanged prefix metadata is also shared through `Arc`;
- one native cache key per served CK message;
- encoded OpenCode chunks as `Arc<Value>`, including each chunk's consumed CK range;
- per-message sidecar digests and hit/miss counters.

A tail delta may reuse sidecar metadata only when its validated `after` fingerprint matches the cache entry and its `native_replace_from` frontier is in range. Full-array requests re-read and hash their sidecar metadata even when the opaque fingerprint repeats, so a message metadata mutation cannot hide behind a stale caller fingerprint.

The first changed served-message key determines the suffix to encode. The restart point backs up one CK message and then snaps to the beginning of the containing encoded chunk. This preserves adjacent fresh tool pairs and collapsed synthetic todo pairs. Cached prefix values are shared by `Arc`; only suffix values are allocated and encoded. The whole combined array still passes the duplicate-tool-use assertion.

## Key fields

The session context key contains:

- session id;
- serializer profile and profile render epoch;
- render configuration;
- renderer transition-consumed salt.

Each served-message key hashes:

- the serialized-output cache identity produced by `message_output_identity` for tail messages;
- the canonical CK message hash as a byte-level backstop;
- served position;
- full sidecar/message metadata digest;
- message tag number;
- reasoning-clear eligibility;
- mutation-exemption state for the live assistant or lineage anchor.

The sidecar digest covers retained raw OpenCode fields and block metadata, not only CK-visible content.

## Invalidation matrix

| Change | Fence that invalidates reuse |
| --- | --- |
| Fold or m0/m1 byte change | Canonical message hash; changed prefix position restarts encoding |
| Coverage advance/removal | Message sequence/position mismatch |
| Frozen reduction or reasoning healing | Serialized-output identity plus canonical hash |
| Synthetic todo insertion, move, replacement, or removal | Message sequence keys and chunk-boundary restart |
| Renderer transition salt | Session context key |
| Durable revert epoch | Session entry eviction |
| Render/profile epoch or profile change | Session context key |
| Tag mutation | Per-message tag number and CK output identity |
| Reasoning watermark or mid-turn effect | Per-message reasoning-clear eligibility and mutation exemption |
| Sidecar/raw/provider metadata change | Per-message sidecar digest |
| Tail append/replace | Validated fingerprint frontier reuses only the unchanged sidecar prefix; changed output suffix is encoded |

## Differential and live observability

Tests always run a full native encode after the incremental path and compare serialized JSON bytes. Debug module processes can enable the same assertion with `MC_NATIVE_ATTACHMENT_DIFFERENTIAL=1`; the real-daemon suite does so. A mutation test deliberately omits the sidecar digest from cache-key derivation and proves the differential assertion fails. Regression coverage also exercises frozen reductions, collapsed synthetic todo pairs, compaction markers, reasoning clearing, every invalidation class, and duplicate tool-use detection.

The pass timing record retains `post_attach_ms` and adds `native_cache_reused_messages` and `native_cache_encoded_messages`, allowing live traces to distinguish a genuinely incremental pass from a fast full encode.
