# DeepSeek V4 Flash: full vs light prompt behavior

Route: `ollama-cloud/deepseek-v4-flash` (request model `deepseek-v4-flash`)

Both presets received the same tagged transcript and real `ctx_reduce` tool schema. Passing requires one real tool call with IDs 103 and 105, no §N§ imitation, and no fabricated dropped marker.

## full

Result: **PASS**

```json
{
  "model": "ollama-cloud/deepseek-v4-flash",
  "requestedModel": "deepseek-v4-flash",
  "preset": "full",
  "content": "",
  "toolCalls": [
    {
      "id": "call_bv1qt0t3",
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
    "promptTokens": 2275,
    "completionTokens": 47
  }
}
```

## light

Result: **PASS**

```json
{
  "model": "ollama-cloud/deepseek-v4-flash",
  "requestedModel": "deepseek-v4-flash",
  "preset": "light",
  "content": "",
  "toolCalls": [
    {
      "id": "call_o9yyogjb",
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
    "promptTokens": 1709,
    "completionTokens": 47
  }
}
```
