---
title: Memory mural
description: An experimental single image of overflow project memories, injected into the cached context baseline when the model can see images.
---

The **memory mural** is an experimental feature that turns project memories that did not fit the text injection budget into **one deterministic PNG**. When enabled, and when the active model accepts images, that image is attached to the cached context baseline on a hard cache fold so the agent can still "see" the overflow pool without spending the full text budget on every memory.

## What it is

- **One image** of overflow memories — not a gallery, not a per-memory thumbnail.
- **Sized to content** — height grows with how many compressed cues are on the mural; width is fixed.
- **Priced like a vision tile grid** — roughly `ceil(width / 28) × ceil(height / 28)` tokens, the usual vision-tile accounting providers use.
- **Only when the text pool overflows** — if every active memory already fits the text budget, no mural is rendered or injected.
- **Deterministic** — the same cue set always produces the same PNG. No author model paints the image.

The mural is a complement to the text `<project-memory>` block, not a replacement. High-priority memories still appear as text; the mural carries what the budget trimmed away.

## When it injects

Injection is gated on all of the following:

1. **`experimental.mural.enabled`** is `true`.
2. The **outgoing model accepts images** (vision capability from provider metadata). Unknown capability fails closed: text-only baseline, no error.
3. Enough memories have **current compressed cues** (coverage gate — see below).
4. There is a non-empty **overflow set** after the normal memory text budget trim.

The image is resolved and attached only when the cached context baseline is **rebuilt** (a hard fold: model change, system-prompt change, idle past cache TTL, and similar). Ordinary defer turns **replay the same baked-in image bytes** so a background cue update cannot silently swap the picture mid-session and bust the prompt cache.

## Compress-cues (dreamer task)

Each memory needs a short **cue** before it can appear on the mural. The dreamer's **compress-cues** task:

- Runs per memory (not one giant prompt for the whole pool).
- Compresses the full memory text into a mural-sized line.
- **Caches by content hash** — unchanged memories are not re-compressed.
- Uses **`experimental.mural.model`** when set, otherwise the dreamer model ladder.

Until a memory has a current cue, it is skipped for mural selection even if it overflowed the text budget.

## Coverage gate

The mural does not render from a half-empty cue pool. Rendering is skipped until either:

- a minimum number of active/permanent memories have current cues, or
- a minimum fraction of that pool is cued

(whichever the implementation threshold is — both exist so small and large projects behave sensibly). Below the gate, the baseline stays text-only.

## Config

```jsonc
{
  "experimental": {
    "mural": {
      "enabled": true,
      // Optional: model for compress-cues only. The PNG itself is deterministic.
      "model": "anthropic/claude-haiku-4-5"
    }
  }
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `experimental.mural.enabled` | `false` | Master switch for mural injection and compress-cues. |
| `experimental.mural.model` | — | Model used by compress-cues. Falls back to the dreamer model when unset. |

## Requirements and limits

- **Vision model required** for the image part. Non-vision models get the same text baseline as with the feature off (no mural marker, no image).
- **Experimental** — opt-in; defaults off.
- Works on **OpenCode and Pi** with the same config and shared store. The image envelope differs by harness (file part vs native image content); the PNG and the fold/replay rules match.

## How it connects

- [Memory](/concepts/memory/) — text injection budget and categories the mural draws from.
- [Dreamer](/concepts/dreamer/) — hosts the compress-cues task on its schedule.
- [Cache architecture](/concepts/cache-architecture/) — why the mural only swaps on a hard fold.
- [Configuration](/reference/configuration/) — full key reference.
