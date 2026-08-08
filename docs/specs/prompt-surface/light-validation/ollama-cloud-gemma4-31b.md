# Gemma 4 31B: full vs light prompt behavior

Route: `ollama-cloud/gemma4:31b` (request model `gemma4:31b`)

Both presets received the same tagged transcript and real `ctx_reduce` tool schema. Passing requires one real tool call with IDs 103 and 105, no §N§ imitation, and no fabricated dropped marker.

## full

Result: **PASS**

```json
{
  "model": "ollama-cloud/gemma4:31b",
  "requestedModel": "gemma4:31b",
  "preset": "full",
  "content": "",
  "toolCalls": [
    {
      "id": "call_dek5c5sp",
      "function": {
        "index": 0,
        "name": "ctx_reduce",
        "arguments": {
          "drop": "103,105"
        }
      }
    }
  ],
  "drop": "103,105",
  "checks": {
    "oneRealCtxReduceCall": true,
    "correctDropShape": true,
    "noTagImitation": true,
    "noFabricatedDroppedMarker": true
  },
  "passed": true,
  "usage": {
    "promptTokens": 2149,
    "completionTokens": 21
  }
}
```

## light

Result: **PASS**

```json
{
  "model": "ollama-cloud/gemma4:31b",
  "requestedModel": "gemma4:31b",
  "preset": "light",
  "content": "",
  "toolCalls": [
    {
      "id": "call_2yonjblq",
      "function": {
        "index": 0,
        "name": "ctx_reduce",
        "arguments": {
          "drop": "103,105"
        }
      }
    }
  ],
  "drop": "103,105",
  "checks": {
    "oneRealCtxReduceCall": true,
    "correctDropShape": true,
    "noTagImitation": true,
    "noFabricatedDroppedMarker": true
  },
  "passed": true,
  "usage": {
    "promptTokens": 1548,
    "completionTokens": 21
  }
}
```
