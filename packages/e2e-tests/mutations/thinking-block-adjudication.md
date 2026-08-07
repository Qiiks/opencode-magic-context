# Thinking-block safety adjudication

The suite was invoked serially 20 times. This artifact records the observed
verdict without changing the suite or the harness to make a failure disappear.

- Command: MC_E2E_MODE=ts NODE_ENV="" bun test --timeout 600000 --max-concurrency=1 tests/thinking-block-safety.test.ts
- Pass count: 17
- Fail count: 3
- Verdict: Flake: 3 of 20 invocations failed while 17 passed; the mixed result is intermittent. All three failures were the same Bug B assertion: the generated `<ctx-search-hint>` preview still contained `ERROR: call_failed at line 42.`. The other two cases passed, and no test or product code was changed.

## Failure output

### Invocation "1" (exit "1")

```text
bun test v1.3.14 (0d9b296a)

tests/thinking-block-safety.test.ts:
(pass) thinking-block safety (Anthropic 400 regression) > Bug A: nudge anchor on a thinking-bearing assistant > does not inject nudge <instruction> text into an assistant that has a thinking block [8996.18ms]
346 |
347 |                 // The paste shell renders as the canonical placeholder.
348 |                 expect(allUserText).toMatch(/\[dropped \u00a7\d+\u00a7\]/);
349 |                 // The raw paste body is gone (replaced by the placeholder) — the
350 |                 // user-text preview was removed for prompt-cache stability.
351 |                 expect(allUserText).not.toContain("ERROR: call_failed at line 42.");
                                              ^
error: expect(received).not.toContain(expected)

Expected to not contain: "ERROR: call_failed at line 42."
Received: "<session-history></session-history>\n<session-history-since>(no new content since last materialization)</session-history-since>\n§1§ please explain how the drop logic works\n\n<ctx-search-hint>\nYour memory may contain 1 related fragment:\n- explain how drop logic works\nIf the fragments above seem relevant to the current request, you may run ctx_search to retrieve full context. Otherwise ignore.\n</ctx-search-hint>\n[dropped §3§]\n\n<ctx-search-hint>\nYour memory may contain 1 related fragment:\n- Here is log of failing session: ERROR: call_failed at line 42. ERROR: call_fail…\nIf the fragments above seem relevant to the current request, you may run ctx_search to retrieve full context. Otherwise ignore.\n</ctx-search-hint>\n§5§ what do you think?"

      at <anonymous> (/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/8f93aad09f2535d0/bg_005778f1/packages/e2e-tests/tests/thinking-block-safety.test.ts:351:41)
(fail) thinking-block safety (Anthropic 400 regression) > Bug B: user-message turn boundary preserved when text tag is dropped > keeps the user shell as [dropped §N§] so adjacent assistants are not merged [2054.16ms]
(pass) thinking-block safety (Anthropic 400 regression) > Bug C: file/image part survives when companion text is dropped > keeps a user message with an image part even after its text tag is dropped [573.14ms]

 2 pass
 1 fail
 24 expect() calls
Ran 3 tests across 1 file. [13.08s]
```

### Invocation "2" (exit "1")

```text
bun test v1.3.14 (0d9b296a)

tests/thinking-block-safety.test.ts:
(pass) thinking-block safety (Anthropic 400 regression) > Bug A: nudge anchor on a thinking-bearing assistant > does not inject nudge <instruction> text into an assistant that has a thinking block [10021.65ms]
346 |
347 |                 // The paste shell renders as the canonical placeholder.
348 |                 expect(allUserText).toMatch(/\[dropped \u00a7\d+\u00a7\]/);
349 |                 // The raw paste body is gone (replaced by the placeholder) — the
350 |                 // user-text preview was removed for prompt-cache stability.
351 |                 expect(allUserText).not.toContain("ERROR: call_failed at line 42.");
                                              ^
error: expect(received).not.toContain(expected)

Expected to not contain: "ERROR: call_failed at line 42."
Received: "<session-history></session-history>\n<session-history-since>(no new content since last materialization)</session-history-since>\n§1§ please explain how the drop logic works\n[dropped §3§]\n\n<ctx-search-hint>\nYour memory may contain 1 related fragment:\n- Here is log of failing session: ERROR: call_failed at line 42. ERROR: call_fail…\nIf the fragments above seem relevant to the current request, you may run ctx_search to retrieve full context. Otherwise ignore.\n</ctx-search-hint>\n§5§ what do you think?"

      at <anonymous> (/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/8f93aad09f2535d0/bg_005778f1/packages/e2e-tests/tests/thinking-block-safety.test.ts:351:41)
(fail) thinking-block safety (Anthropic 400 regression) > Bug B: user-message turn boundary preserved when text tag is dropped > keeps the user shell as [dropped §N§] so adjacent assistants are not merged [1744.02ms]
(pass) thinking-block safety (Anthropic 400 regression) > Bug C: file/image part survives when companion text is dropped > keeps a user message with an image part even after its text tag is dropped [654.27ms]

 2 pass
 1 fail
 24 expect() calls
Ran 3 tests across 1 file. [14.55s]
```

### Invocation "3" (exit "1")

```text
bun test v1.3.14 (0d9b296a)

tests/thinking-block-safety.test.ts:
(pass) thinking-block safety (Anthropic 400 regression) > Bug A: nudge anchor on a thinking-bearing assistant > does not inject nudge <instruction> text into an assistant that has a thinking block [11147.19ms]
346 |
347 |                 // The paste shell renders as the canonical placeholder.
348 |                 expect(allUserText).toMatch(/\[dropped \u00a7\d+\u00a7\]/);
349 |                 // The raw paste body is gone (replaced by the placeholder) — the
350 |                 // user-text preview was removed for prompt-cache stability.
351 |                 expect(allUserText).not.toContain("ERROR: call_failed at line 42.");
                                              ^
error: expect(received).not.toContain(expected)

Expected to not contain: "ERROR: call_failed at line 42."
Received: "<session-history></session-history>\n<session-history-since>(no new content since last materialization)</session-history-since>\n§1§ please explain how the drop logic works\n[dropped §3§]\n\n<ctx-search-hint>\nYour memory may contain 1 related fragment:\n- Here is log of failing session: ERROR: call_failed at line 42. ERROR: call_fail…\nIf the fragments above seem relevant to the current request, you may run ctx_search to retrieve full context. Otherwise ignore.\n</ctx-search-hint>\n§5§ what do you think?"

      at <anonymous> (/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/8f93aad09f2535d0/bg_005778f1/packages/e2e-tests/tests/thinking-block-safety.test.ts:351:41)
(fail) thinking-block safety (Anthropic 400 regression) > Bug B: user-message turn boundary preserved when text tag is dropped > keeps the user shell as [dropped §N§] so adjacent assistants are not merged [636.23ms]
(pass) thinking-block safety (Anthropic 400 regression) > Bug C: file/image part survives when companion text is dropped > keeps a user message with an image part even after its text tag is dropped [951.75ms]

 2 pass
 1 fail
 24 expect() calls
Ran 3 tests across 1 file. [14.39s]
```
