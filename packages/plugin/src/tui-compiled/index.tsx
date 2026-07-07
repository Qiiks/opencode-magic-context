import { memo as _$memo } from "opentui:runtime-module:%40opentui%2Fsolid";
import { createTextNode as _$createTextNode } from "opentui:runtime-module:%40opentui%2Fsolid";
import { effect as _$effect } from "opentui:runtime-module:%40opentui%2Fsolid";
import { insertNode as _$insertNode } from "opentui:runtime-module:%40opentui%2Fsolid";
import { insert as _$insert } from "opentui:runtime-module:%40opentui%2Fsolid";
import { setProp as _$setProp } from "opentui:runtime-module:%40opentui%2Fsolid";
import { createElement as _$createElement } from "opentui:runtime-module:%40opentui%2Fsolid";
import { createComponent as _$createComponent } from "opentui:runtime-module:%40opentui%2Fsolid";
/** @jsxImportSource @opentui/solid */
// @ts-nocheck
import { createMemo } from "opentui:runtime-module:solid-js";
import { createSidebarContentSlot, kickRecompProgressRefresh } from "./slots/sidebar-content";
import packageJson from "../../package.json";
import { closeRpc, dismissUpgradeReminder, getAnnouncement, getCompartmentCount, getRpcGeneration, initRpcClient, loadEmbedDetail, loadStatusDetail, loadToastDurationMs, markAnnounced, requestRecomp, requestUpgrade } from "./data/context-db";
import { startNotificationSocket, stopNotificationSocket } from "./data/notification-socket";
import { formatThresholdPercent } from "../shared/format-threshold";
import { detectConflicts } from "../shared/conflict-detector";
import { fixConflicts } from "../shared/conflict-fixer";
const DEFAULT_TOAST_DURATION_MS = 5000;
let unifiedToastDurationMs = DEFAULT_TOAST_DURATION_MS;
async function refreshToastDurationMs() {
  try {
    const resolved = await loadToastDurationMs();
    if (typeof resolved === "number" && Number.isFinite(resolved)) {
      unifiedToastDurationMs = resolved;
    }
  } catch {
    // Keep the current value; the next poll/startup can retry.
  }
}
function getToastDurationMs() {
  return unifiedToastDurationMs;
}
function showToast(api, input) {
  const duration = typeof input.durationOverrideMs === "number" && Number.isFinite(input.durationOverrideMs) ? input.durationOverrideMs : getToastDurationMs();
  // toast_duration_ms = 0 disables Magic Context toasts entirely. An explicit
  // positive per-call override (e.g. restart-required) still shows; only a
  // non-positive effective duration suppresses the toast.
  if (!(duration > 0)) {
    return;
  }
  api.ui.toast({
    message: input.message,
    variant: input.variant,
    duration
  });
}
function showConflictDialog(api, directory, reasons, conflicts) {
  api.ui.dialog.replace(() => _$createComponent(api.ui.DialogConfirm, {
    title: "\u26A0\uFE0F Magic Context Disabled",
    get message() {
      return `${reasons.join("\n")}\n\nFix these conflicts automatically?`;
    },
    onConfirm: () => {
      const actions = fixConflicts(directory, conflicts);
      const actionSummary = actions.length > 0 ? actions.map(a => `• ${a}`).join("\n") : "No changes needed";
      // DialogConfirm calls dialog.clear() after onConfirm, so defer the next dialog
      setTimeout(() => {
        api.ui.dialog.replace(() => _$createComponent(api.ui.DialogAlert, {
          title: "\u2705 Configuration Fixed",
          message: `${actionSummary}\n\nPlease restart OpenCode for changes to take effect.`,
          onConfirm: () => {
            showToast(api, {
              message: "Restart OpenCode to enable Magic Context",
              variant: "warning",
              durationOverrideMs: 10_000
            });
          }
        }));
      }, 50);
    },
    onCancel: () => {
      showToast(api, {
        message: "Magic Context remains disabled. Run: npx @cortexkit/opencode-magic-context@latest doctor",
        variant: "warning"
      });
    }
  }));
}
function fmt(n) {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}K`;
  return String(n);
}
function fmtBytes(n) {
  if (n >= 1_048_576) return `${(n / 1_048_576).toFixed(1)} MB`;
  if (n >= 1_024) return `${Math.round(n / 1_024)} KB`;
  return `${n} B`;
}
function relTime(ms) {
  const d = Date.now() - ms;
  if (d < 60_000) return "just now";
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m ago`;
  if (d < 86_400_000) return `${Math.floor(d / 3_600_000)}h ago`;
  return `${Math.floor(d / 86_400_000)}d ago`;
}
function getSessionId(api) {
  try {
    const route = api.route.current;
    if (route?.name === "session" && route.params?.sessionID) {
      return route.params.sessionID;
    }
  } catch {
    // ignore
  }
  return null;
}
const R = props => (() => {
  var _el$ = _$createElement("box"),
    _el$2 = _$createElement("text"),
    _el$3 = _$createElement("text");
  _$insertNode(_el$, _el$2);
  _$insertNode(_el$, _el$3);
  _$setProp(_el$, "width", "100%");
  _$setProp(_el$, "flexDirection", "row");
  _$setProp(_el$, "justifyContent", "space-between");
  _$insert(_el$2, () => props.l);
  _$insert(_el$3, () => props.v);
  _$effect(_p$ => {
    var _v$ = props.t.textMuted,
      _v$2 = props.fg ?? props.t.text;
    _v$ !== _p$.e && (_p$.e = _$setProp(_el$2, "fg", _v$, _p$.e));
    _v$2 !== _p$.t && (_p$.t = _$setProp(_el$3, "fg", _v$2, _p$.t));
    return _p$;
  }, {
    e: undefined,
    t: undefined
  });
  return _el$;
})();
const StatusDialog = props => {
  const theme = createMemo(() => props.api.theme.current);
  const t = () => theme();
  const s = () => props.s;

  // Prefer the RPC-provided model context limit (what the sidebar shows) so the
  // two surfaces never disagree. Fall back to deriving from usage% only when the
  // RPC limit is absent (0) — and that derivation is itself undefined at 0%, so
  // it stays "?" rather than showing a number inconsistent with the sidebar.
  const contextLimit = () => s().contextLimit > 0 ? s().contextLimit : s().usagePercentage > 0 ? Math.round(s().inputTokens / (s().usagePercentage / 100)) : 0;
  const elapsed = () => s().lastResponseTime > 0 ? Date.now() - s().lastResponseTime : 0;

  // Token breakdown segments — same colors as sidebar. Kept in sync with
  // slots/sidebar-content.tsx so the status dialog and sidebar read identically.
  const COLORS = {
    // Cool / structured — injected by the plugin into message[0]
    system: "#c084fc",
    docs: "#22d3ee",
    compartments: "#60a5fa",
    facts: "#fbbf24",
    memories: "#34d399",
    profile: "#a3e635",
    // Warm / user-facing — chat and tool traffic
    conversation: "#f87171",
    toolCalls: "#fb923c",
    toolDefs: "#f472b6"
  };
  const breakdownSegments = () => {
    const d = s();
    const total = d.inputTokens || 1;
    const segs = [];
    if (d.systemPromptTokens > 0) segs.push({
      label: "System",
      tokens: d.systemPromptTokens,
      color: COLORS.system
    });
    if (d.docsTokens > 0) segs.push({
      label: "Docs",
      tokens: d.docsTokens,
      color: COLORS.docs
    });
    if (d.compartmentTokens > 0) segs.push({
      label: "Compartments",
      tokens: d.compartmentTokens,
      color: COLORS.compartments,
      detail: `(${d.compartmentCount})`
    });
    if (d.factTokens > 0) segs.push({
      label: "Facts",
      tokens: d.factTokens,
      color: COLORS.facts
    });
    if (d.memoryTokens > 0) segs.push({
      label: "Memories",
      tokens: d.memoryTokens,
      color: COLORS.memories,
      detail: `(${d.memoryBlockCount})`
    });
    if (d.profileTokens > 0) segs.push({
      label: "User Profile",
      tokens: d.profileTokens,
      color: COLORS.profile
    });
    if (d.conversationTokens > 0) segs.push({
      label: "Conversation",
      tokens: d.conversationTokens,
      color: COLORS.conversation
    });
    if (d.toolCallTokens > 0) segs.push({
      label: "Tool Calls",
      tokens: d.toolCallTokens,
      color: COLORS.toolCalls
    });
    if (d.toolDefinitionTokens > 0) segs.push({
      label: "Tool Defs",
      tokens: d.toolDefinitionTokens,
      color: COLORS.toolDefs
    });
    return {
      segs,
      total
    };
  };

  // The status-dialog breakdown bar uses flex layout (same approach as the
  // sidebar breakdown). Each segment becomes a colored box with
  // flexGrow=tokens and flexBasis=0, parent has width="100%", so opentui
  // distributes the dialog's full width proportionally regardless of the
  // dialog's actual rendered width.
  const barSegments = () => breakdownSegments().segs.filter(seg => seg.tokens > 0);
  return (() => {
    var _el$4 = _$createElement("box"),
      _el$5 = _$createElement("box"),
      _el$6 = _$createElement("text"),
      _el$7 = _$createElement("b"),
      _el$9 = _$createElement("text"),
      _el$0 = _$createTextNode(`v`),
      _el$1 = _$createElement("box"),
      _el$10 = _$createElement("text"),
      _el$11 = _$createElement("b"),
      _el$12 = _$createTextNode(`%`),
      _el$13 = _$createTextNode(` / `),
      _el$14 = _$createTextNode(`%`),
      _el$15 = _$createElement("text"),
      _el$16 = _$createTextNode(` / `),
      _el$17 = _$createTextNode(` tokens`),
      _el$18 = _$createElement("box"),
      _el$19 = _$createElement("box"),
      _el$20 = _$createElement("box"),
      _el$21 = _$createElement("box"),
      _el$22 = _$createElement("text"),
      _el$23 = _$createElement("b"),
      _el$25 = _$createElement("box"),
      _el$26 = _$createElement("text"),
      _el$27 = _$createElement("b"),
      _el$29 = _$createElement("box"),
      _el$30 = _$createElement("text"),
      _el$31 = _$createElement("b"),
      _el$33 = _$createElement("box"),
      _el$34 = _$createElement("text"),
      _el$35 = _$createElement("b"),
      _el$37 = _$createElement("box"),
      _el$38 = _$createElement("text"),
      _el$39 = _$createElement("b"),
      _el$41 = _$createElement("box"),
      _el$42 = _$createElement("text"),
      _el$43 = _$createElement("b"),
      _el$45 = _$createElement("box"),
      _el$46 = _$createElement("text"),
      _el$47 = _$createElement("b"),
      _el$49 = _$createElement("box"),
      _el$50 = _$createElement("text");
    _$insertNode(_el$4, _el$5);
    _$insertNode(_el$4, _el$1);
    _$insertNode(_el$4, _el$18);
    _$insertNode(_el$4, _el$19);
    _$insertNode(_el$4, _el$20);
    _$insertNode(_el$4, _el$49);
    _$setProp(_el$4, "flexDirection", "column");
    _$setProp(_el$4, "width", "100%");
    _$setProp(_el$4, "paddingLeft", 2);
    _$setProp(_el$4, "paddingRight", 2);
    _$setProp(_el$4, "paddingTop", 1);
    _$setProp(_el$4, "paddingBottom", 1);
    _$insertNode(_el$5, _el$6);
    _$insertNode(_el$5, _el$9);
    _$setProp(_el$5, "justifyContent", "center");
    _$setProp(_el$5, "width", "100%");
    _$setProp(_el$5, "marginBottom", 1);
    _$setProp(_el$5, "flexDirection", "row");
    _$setProp(_el$5, "gap", 2);
    _$insertNode(_el$6, _el$7);
    _$insertNode(_el$7, _$createTextNode(`⚡ Magic Context Status`));
    _$insertNode(_el$9, _el$0);
    _$insert(_el$9, () => packageJson.version, null);
    _$insertNode(_el$1, _el$10);
    _$insertNode(_el$1, _el$15);
    _$setProp(_el$1, "flexDirection", "row");
    _$setProp(_el$1, "justifyContent", "space-between");
    _$setProp(_el$1, "width", "100%");
    _$insertNode(_el$10, _el$11);
    _$insertNode(_el$10, _el$13);
    _$insertNode(_el$10, _el$14);
    _$insertNode(_el$11, _el$12);
    _$insert(_el$11, () => s().usagePercentage.toFixed(1), _el$12);
    _$insert(_el$10, () => formatThresholdPercent(s().executeThreshold), _el$14);
    _$insertNode(_el$15, _el$16);
    _$insertNode(_el$15, _el$17);
    _$insert(_el$15, () => fmt(s().inputTokens), _el$16);
    _$insert(_el$15, (() => {
      var _c$ = _$memo(() => contextLimit() > 0);
      return () => _c$() ? fmt(contextLimit()) : "?";
    })(), _el$17);
    _$setProp(_el$18, "width", "100%");
    _$setProp(_el$18, "flexDirection", "row");
    _$setProp(_el$18, "height", 1);
    _$insert(_el$18, () => barSegments().map(seg => (() => {
      var _el$52 = _$createElement("box");
      _$setProp(_el$52, "flexBasis", 0);
      _$setProp(_el$52, "height", 1);
      _$effect(_p$ => {
        var _v$13 = seg.label,
          _v$14 = Math.max(1, seg.tokens),
          _v$15 = seg.color;
        _v$13 !== _p$.e && (_p$.e = _$setProp(_el$52, "key", _v$13, _p$.e));
        _v$14 !== _p$.t && (_p$.t = _$setProp(_el$52, "flexGrow", _v$14, _p$.t));
        _v$15 !== _p$.a && (_p$.a = _$setProp(_el$52, "backgroundColor", _v$15, _p$.a));
        return _p$;
      }, {
        e: undefined,
        t: undefined,
        a: undefined
      });
      return _el$52;
    })()));
    _$setProp(_el$19, "flexDirection", "column");
    _$insert(_el$19, () => breakdownSegments().segs.map(seg => {
      const pct = (seg.tokens / breakdownSegments().total * 100).toFixed(1);
      return (() => {
        var _el$53 = _$createElement("box"),
          _el$54 = _$createElement("text"),
          _el$55 = _$createTextNode(` `),
          _el$56 = _$createElement("text"),
          _el$57 = _$createTextNode(` (`),
          _el$58 = _$createTextNode(`%)`);
        _$insertNode(_el$53, _el$54);
        _$insertNode(_el$53, _el$56);
        _$setProp(_el$53, "width", "100%");
        _$setProp(_el$53, "flexDirection", "row");
        _$setProp(_el$53, "justifyContent", "space-between");
        _$insertNode(_el$54, _el$55);
        _$insert(_el$54, () => seg.label, _el$55);
        _$insert(_el$54, () => seg.detail ?? "", null);
        _$insertNode(_el$56, _el$57);
        _$insertNode(_el$56, _el$58);
        _$insert(_el$56, () => fmt(seg.tokens), _el$57);
        _$insert(_el$56, pct, _el$58);
        _$effect(_p$ => {
          var _v$16 = seg.label,
            _v$17 = seg.color,
            _v$18 = t().textMuted;
          _v$16 !== _p$.e && (_p$.e = _$setProp(_el$53, "key", _v$16, _p$.e));
          _v$17 !== _p$.t && (_p$.t = _$setProp(_el$54, "fg", _v$17, _p$.t));
          _v$18 !== _p$.a && (_p$.a = _$setProp(_el$56, "fg", _v$18, _p$.a));
          return _p$;
        }, {
          e: undefined,
          t: undefined,
          a: undefined
        });
        return _el$53;
      })();
    }));
    _$insert(_el$4, (() => {
      var _c$2 = _$memo(() => !!s().recompProgress);
      return () => _c$2() && (() => {
        const p = s().recompProgress;
        // Label follows the flow that started the run, so a plain
        // /ctx-recomp never reads as an "Upgrade" (dogfood 2026-06-04).
        const verb = p.kind === "upgrade" ? "Upgrade" : p.kind === "embed" ? "Embed" : "Recomp";
        return (() => {
          var _el$59 = _$createElement("box"),
            _el$60 = _$createElement("text"),
            _el$61 = _$createElement("b");
          _$insertNode(_el$59, _el$60);
          _$setProp(_el$59, "marginTop", 1);
          _$setProp(_el$59, "width", "100%");
          _$setProp(_el$59, "flexDirection", "column");
          _$insertNode(_el$60, _el$61);
          _$insert(_el$61, verb);
          _$insert(_el$59, () => {
            if (p.phase === "recomp") {
              const frac = p.totalMessages > 0 ? p.processedMessages / p.totalMessages : 0;
              const width = 24;
              const filled = Math.round(Math.max(0, Math.min(1, frac)) * width);
              const bar = p.totalMessages > 0 ? `[${"█".repeat(filled)}${"░".repeat(width - filled)}]` : "(starting…)";
              const activeLabel = p.kind === "upgrade" ? "upgrading" : p.kind === "embed" ? "embedding" : "comparting";
              return [_$createComponent(R, {
                get t() {
                  return t();
                },
                l: activeLabel,
                get v() {
                  return _$memo(() => p.totalMessages > 0)() ? `${bar} ${Math.round(frac * 100)}%` : bar;
                },
                get fg() {
                  return t().warning;
                }
              }), _$memo(() => _$memo(() => !!p.note)() ? _$createComponent(R, {
                get t() {
                  return t();
                },
                l: "Status",
                get v() {
                  return p.note;
                },
                get fg() {
                  return t().textMuted;
                }
              }) : null), _$memo(() => _$memo(() => p.kind === "embed")() ? _$createComponent(R, {
                get t() {
                  return t();
                },
                l: "Compartments",
                get v() {
                  return `${p.processedMessages}/${p.totalMessages} embedded`;
                },
                get fg() {
                  return t().textMuted;
                }
              }) : _$createComponent(R, {
                get t() {
                  return t();
                },
                l: "Compartments",
                get v() {
                  return `${p.compartmentsCreated} (${p.passCount} pass${p.passCount === 1 ? "" : "es"})`;
                },
                get fg() {
                  return t().textMuted;
                }
              }))];
            }
            if (p.phase === "migration") return _$createComponent(R, {
              get t() {
                return t();
              },
              l: "Status",
              get v() {
                return p.note ?? "Migrating memories ⟳";
              },
              get fg() {
                return t().warning;
              }
            });
            if (p.phase === "done") return _$createComponent(R, {
              get t() {
                return t();
              },
              l: "Status",
              v: `✓ ${verb} complete`,
              get fg() {
                return t().accent;
              }
            });
            if (p.phase === "skipped") return _$createComponent(R, {
              get t() {
                return t();
              },
              l: "Status",
              get v() {
                return p.message ?? `${verb} stopped early`;
              },
              get fg() {
                return t().textMuted;
              }
            });
            return _$createComponent(R, {
              get t() {
                return t();
              },
              l: "Status",
              get v() {
                return `✗ ${verb} failed${p.message ? `: ${p.message}` : ""}`;
              },
              get fg() {
                return t().error;
              }
            });
          }, null);
          _$effect(_$p => _$setProp(_el$60, "fg", t().text, _$p));
          return _el$59;
        })();
      })();
    })(), _el$20);
    _$insertNode(_el$20, _el$21);
    _$insertNode(_el$20, _el$37);
    _$setProp(_el$20, "flexDirection", "row");
    _$setProp(_el$20, "width", "100%");
    _$setProp(_el$20, "marginTop", 1);
    _$setProp(_el$20, "gap", 4);
    _$insertNode(_el$21, _el$22);
    _$insertNode(_el$21, _el$25);
    _$insertNode(_el$21, _el$29);
    _$insertNode(_el$21, _el$33);
    _$setProp(_el$21, "flexDirection", "column");
    _$setProp(_el$21, "flexGrow", 1);
    _$setProp(_el$21, "flexBasis", 0);
    _$insertNode(_el$22, _el$23);
    _$insertNode(_el$23, _$createTextNode(`Tags`));
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Active",
      get v() {
        return `${s().activeTags} (~${fmtBytes(s().activeBytes)})`;
      }
    }), _el$25);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Dropped",
      get v() {
        return String(s().droppedTags);
      }
    }), _el$25);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Total",
      get v() {
        return String(s().totalTags);
      },
      get fg() {
        return t().textMuted;
      }
    }), _el$25);
    _$insertNode(_el$25, _el$26);
    _$setProp(_el$25, "marginTop", 1);
    _$insertNode(_el$26, _el$27);
    _$insertNode(_el$27, _$createTextNode(`Pending Queue`));
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Drops",
      get v() {
        return String(s().pendingOpsCount);
      },
      get fg() {
        return _$memo(() => s().pendingOpsCount > 0)() ? t().warning : t().textMuted;
      }
    }), _el$29);
    _$insertNode(_el$29, _el$30);
    _$setProp(_el$29, "marginTop", 1);
    _$insertNode(_el$30, _el$31);
    _$insertNode(_el$31, _$createTextNode(`Cache TTL`));
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Configured",
      get v() {
        return s().cacheTtl;
      }
    }), _el$33);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Last response",
      get v() {
        return _$memo(() => s().lastResponseTime > 0)() ? `${Math.round(elapsed() / 1000)}s ago` : "never";
      }
    }), _el$33);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Remaining",
      get v() {
        return _$memo(() => !!s().cacheExpired)() ? "expired" : `${Math.round(s().cacheRemainingMs / 1000)}s`;
      },
      get fg() {
        return _$memo(() => !!s().cacheExpired)() ? t().warning : t().textMuted;
      }
    }), _el$33);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Auto-execute",
      get v() {
        return _$memo(() => !!s().cacheExpired)() ? "yes (expired)" : `at TTL or ≥${formatThresholdPercent(s().executeThreshold)}%`;
      },
      get fg() {
        return t().textMuted;
      }
    }), _el$33);
    _$insertNode(_el$33, _el$34);
    _$setProp(_el$33, "marginTop", 1);
    _$insertNode(_el$34, _el$35);
    _$insertNode(_el$35, _$createTextNode(`Memory`));
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Active",
      get v() {
        return String(s().memoryCount);
      },
      get fg() {
        return t().accent;
      }
    }), null);
    _$insert(_el$21, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Injected",
      get v() {
        return String(s().memoryBlockCount);
      },
      get fg() {
        return t().textMuted;
      }
    }), null);
    _$insertNode(_el$37, _el$38);
    _$insertNode(_el$37, _el$41);
    _$insertNode(_el$37, _el$45);
    _$setProp(_el$37, "flexDirection", "column");
    _$setProp(_el$37, "flexGrow", 1);
    _$setProp(_el$37, "flexBasis", 0);
    _$insertNode(_el$38, _el$39);
    _$insertNode(_el$39, _$createTextNode(`Reductions`));
    _$insert(_el$37, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Execute threshold",
      get v() {
        return `${formatThresholdPercent(s().executeThreshold)}%`;
      }
    }), _el$41);
    _$insert(_el$37, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Last reduce anchor",
      get v() {
        return `${fmt(s().lastNudgeTokens)} tok`;
      }
    }), _el$41);
    _$insertNode(_el$41, _el$42);
    _$setProp(_el$41, "marginTop", 1);
    _$insertNode(_el$42, _el$43);
    _$insertNode(_el$43, _$createTextNode(`Context Details`));
    _$insert(_el$37, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Protected tags",
      get v() {
        return String(s().protectedTagCount);
      },
      get fg() {
        return t().textMuted;
      }
    }), _el$45);
    _$insert(_el$37, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "Subagent",
      get v() {
        return s().isSubagent ? "yes" : "no";
      },
      get fg() {
        return t().textMuted;
      }
    }), _el$45);
    _$insertNode(_el$45, _el$46);
    _$setProp(_el$45, "marginTop", 1);
    _$insertNode(_el$46, _el$47);
    _$insertNode(_el$47, _$createTextNode(`History Compression`));
    _$insert(_el$37, _$createComponent(R, {
      get t() {
        return t();
      },
      l: "History block",
      get v() {
        return `~${fmt(s().historyBlockTokens)} tok`;
      }
    }), null);
    _$insert(_el$37, (() => {
      var _c$3 = _$memo(() => s().compressionBudget != null);
      return () => _c$3() && _$createComponent(R, {
        get t() {
          return t();
        },
        l: "Budget",
        get v() {
          return `~${fmt(s().compressionBudget)} tok (${s().compressionUsage} used)`;
        }
      });
    })(), null);
    _$insert(_el$37, (() => {
      var _c$4 = _$memo(() => !!s().lastDreamerRunAt);
      return () => _c$4() && _$createComponent(R, {
        get t() {
          return t();
        },
        l: "Dreamer",
        get v() {
          return `last ${relTime(s().lastDreamerRunAt)}`;
        },
        get fg() {
          return t().textMuted;
        }
      });
    })(), null);
    _$insert(_el$4, (() => {
      var _c$5 = _$memo(() => !!s().lastTransformError);
      return () => _c$5() && (() => {
        var _el$62 = _$createElement("box"),
          _el$63 = _$createElement("text"),
          _el$64 = _$createTextNode(`⚠ `);
        _$insertNode(_el$62, _el$63);
        _$setProp(_el$62, "marginTop", 1);
        _$setProp(_el$62, "width", "100%");
        _$insertNode(_el$63, _el$64);
        _$insert(_el$63, () => s().lastTransformError, null);
        _$effect(_$p => _$setProp(_el$63, "fg", t().error, _$p));
        return _el$62;
      })();
    })(), _el$49);
    _$insertNode(_el$49, _el$50);
    _$setProp(_el$49, "marginTop", 1);
    _$setProp(_el$49, "justifyContent", "flex-end");
    _$setProp(_el$49, "width", "100%");
    _$insertNode(_el$50, _$createTextNode(`Esc to close`));
    _$effect(_p$ => {
      var _v$3 = t().accent,
        _v$4 = t().textMuted,
        _v$5 = s().usagePercentage >= 80 ? t().error : s().usagePercentage >= 65 ? t().warning : t().accent,
        _v$6 = s().usagePercentage >= 80 ? t().error : s().usagePercentage >= 65 ? t().warning : t().accent,
        _v$7 = t().text,
        _v$8 = t().text,
        _v$9 = t().text,
        _v$0 = t().text,
        _v$1 = t().text,
        _v$10 = t().text,
        _v$11 = t().text,
        _v$12 = t().textMuted;
      _v$3 !== _p$.e && (_p$.e = _$setProp(_el$6, "fg", _v$3, _p$.e));
      _v$4 !== _p$.t && (_p$.t = _$setProp(_el$9, "fg", _v$4, _p$.t));
      _v$5 !== _p$.a && (_p$.a = _$setProp(_el$10, "fg", _v$5, _p$.a));
      _v$6 !== _p$.o && (_p$.o = _$setProp(_el$15, "fg", _v$6, _p$.o));
      _v$7 !== _p$.i && (_p$.i = _$setProp(_el$22, "fg", _v$7, _p$.i));
      _v$8 !== _p$.n && (_p$.n = _$setProp(_el$26, "fg", _v$8, _p$.n));
      _v$9 !== _p$.s && (_p$.s = _$setProp(_el$30, "fg", _v$9, _p$.s));
      _v$0 !== _p$.h && (_p$.h = _$setProp(_el$34, "fg", _v$0, _p$.h));
      _v$1 !== _p$.r && (_p$.r = _$setProp(_el$38, "fg", _v$1, _p$.r));
      _v$10 !== _p$.d && (_p$.d = _$setProp(_el$42, "fg", _v$10, _p$.d));
      _v$11 !== _p$.l && (_p$.l = _$setProp(_el$46, "fg", _v$11, _p$.l));
      _v$12 !== _p$.u && (_p$.u = _$setProp(_el$50, "fg", _v$12, _p$.u));
      return _p$;
    }, {
      e: undefined,
      t: undefined,
      a: undefined,
      o: undefined,
      i: undefined,
      n: undefined,
      s: undefined,
      h: undefined,
      r: undefined,
      d: undefined,
      l: undefined,
      u: undefined
    });
    return _el$4;
  })();
};
function getModelKeyFromMessages(api, sessionId) {
  try {
    const msgs = api.state.session.messages(sessionId);
    // Find the last assistant message with model info
    // AssistantMessage has providerID/modelID as top-level fields
    // UserMessage has model: { providerID, modelID }
    for (let i = msgs.length - 1; i >= 0; i--) {
      const msg = msgs[i];
      if (msg.role === "assistant" && msg.providerID && msg.modelID) {
        return `${msg.providerID}/${msg.modelID}`;
      }
      if (msg.role === "user") {
        const model = msg.model;
        if (model?.providerID && model?.modelID) {
          return `${model.providerID}/${model.modelID}`;
        }
      }
    }
  } catch {
    // messages not available
  }
  return undefined;
}
async function showRecompDialog(api, targetSessionId = getSessionId(api)) {
  const sessionId = targetSessionId;
  if (!sessionId) {
    showToast(api, {
      message: "No active session",
      variant: "warning"
    });
    return false;
  }
  const count = await getCompartmentCount(sessionId);
  // Ack only after the dialog is actually shown for the same active session;
  // route switches while the RPC detail load is in flight must leave it pending.
  if (getSessionId(api) !== sessionId) return false;
  api.ui.dialog.replace(() => _$createComponent(api.ui.DialogConfirm, {
    title: "\u26A0\uFE0F Recomp Confirmation",
    get message() {
      return [count === 0 ? "This session has no compartments yet — recomp will build them from raw history." : `You have ${count} compartments.`, "", "Recomp will regenerate all compartments and facts from raw history.", "This may take a long time and consume significant tokens.", "", "Proceed?"].join("\n");
    },
    onConfirm: () => {
      void requestRecomp(sessionId);
      kickRecompProgressRefresh();
      showToast(api, {
        message: "Recomp requested — historian will start shortly",
        variant: "info"
      });
    },
    onCancel: () => {
      showToast(api, {
        message: "Recomp cancelled",
        variant: "info",
        durationOverrideMs: 3000
      });
    }
  }));
  return true;
}
function showUpgradeDialog(api, resume, targetSessionId = getSessionId(api)) {
  const sessionId = targetSessionId;
  if (!sessionId) {
    // No active session — nothing to upgrade. Silently skip (the server only
    // enqueues this for sessions with legacy compartments, but the TUI may
    // have switched sessions before the poller fired).
    return false;
  }
  if (getSessionId(api) !== sessionId) return false;
  const title = resume ? "🎆 Resume the interrupted upgrade?" : "🎆 Historian V2 is released!";
  const message = resume ? [`An earlier upgrade to the new historian format was interrupted. ${resume.stagedCount} compartment${resume.stagedCount === 1 ? " was" : "s were"} already rebuilt (through message ${resume.stagedThrough}). Resuming continues from where it left off — nothing already rebuilt is reprocessed.`, "", "Resuming will:", "• Rebuild the remaining compartments into the new layered format", "• Re-organize this project's memories into the new taxonomy (once per project)", "", "The historian runs in the background and you can keep working. You can also resume via /ctx-session-upgrade later.", "", "Resume the upgrade now?"].join("\n") : ["This session's compartments are written by the old historian. The session is still usable with its old compartments, however it's strongly advised to upgrade them to the new format. This means every compartment needs to be reprocessed by the new historian, which might take a while depending on how big your session is.", "", "Running the upgrade will:", "• Rebuild this session's compartments into the new layered format", "• Re-organize this project's memories into the new taxonomy (once per project)", "", "The historian runs in the background and you can keep working while older compartments are reprocessed. You can also upgrade via /ctx-session-upgrade later.", "", "Run the upgrade now?"].join("\n");
  api.ui.dialog.replace(() => _$createComponent(api.ui.DialogConfirm, {
    title: title,
    message: message,
    onConfirm: () => {
      // Start the sidebar's recomp self-poll immediately — the RPC
      // call fires no message event, so without this the progress
      // bar wouldn't appear until the upgrade finished.
      kickRecompProgressRefresh();
      showToast(api, {
        message: resume ? "Resuming session upgrade — running in the background" : "Session upgrade started — running in the background",
        variant: "info"
      });
      // Dismiss the durable reminder ONLY after the upgrade request
      // actually started. If requestUpgrade() returns false (RPC /
      // server / db / auth failure, restart race), the session stays
      // legacy — dismissing first would set upgradeRemindedAt and
      // suppress all future reminders for a session that never
      // upgraded. (Resume prompts are staging-driven and unaffected.)
      void requestUpgrade(sessionId).then(started => {
        if (started) void dismissUpgradeReminder(sessionId);
      });
    },
    onCancel: () => {
      // Explicit decline → set the durable stamp so we don't re-prompt
      // on every restart. The fix for stamp-on-display trapping a
      // never-upgraded session (dogfood 2026-05-30) relies on THIS
      // being the only place the TUI path stamps.
      void dismissUpgradeReminder(sessionId);
      showToast(api, {
        message: "Upgrade skipped — run /ctx-session-upgrade anytime",
        variant: "info",
        durationOverrideMs: 4000
      });
    }
  }));
  return true;
}
async function showStatusDialog(api, targetSessionId = getSessionId(api)) {
  const sessionId = targetSessionId;
  if (!sessionId) {
    showToast(api, {
      message: "No active session",
      variant: "warning"
    });
    return false;
  }
  const directory = api.state.path.directory ?? "";
  const modelKey = getModelKeyFromMessages(api, sessionId);
  const detail = await loadStatusDetail(sessionId, directory, modelKey);
  if (getSessionId(api) !== sessionId) return false;
  api.ui.dialog.replace(() => _$createComponent(StatusDialog, {
    api: api,
    s: detail
  }));
  return true;
}
const EmbedDialog = props => {
  const theme = createMemo(() => props.api.theme.current);
  const t = () => theme();
  const lines = () => props.detail.statusText.split("\n");
  return (() => {
    var _el$65 = _$createElement("box"),
      _el$66 = _$createElement("box"),
      _el$67 = _$createElement("text"),
      _el$68 = _$createElement("b");
    _$insertNode(_el$65, _el$66);
    _$setProp(_el$65, "flexDirection", "column");
    _$setProp(_el$65, "width", "100%");
    _$setProp(_el$65, "paddingLeft", 2);
    _$setProp(_el$65, "paddingRight", 2);
    _$setProp(_el$65, "paddingTop", 1);
    _$setProp(_el$65, "paddingBottom", 1);
    _$insertNode(_el$66, _el$67);
    _$setProp(_el$66, "justifyContent", "center");
    _$setProp(_el$66, "width", "100%");
    _$setProp(_el$66, "marginBottom", 1);
    _$insertNode(_el$67, _el$68);
    _$insertNode(_el$68, _$createTextNode(`Embedding`));
    _$insert(_el$65, () => lines().map(line => (() => {
      var _el$70 = _$createElement("text");
      _$insert(_el$70, line);
      _$effect(_$p => _$setProp(_el$70, "fg", t().text, _$p));
      return _el$70;
    })()), null);
    _$effect(_$p => _$setProp(_el$67, "fg", t().accent, _$p));
    return _el$65;
  })();
};
async function showEmbedDialog(api, targetSessionId = getSessionId(api)) {
  const sessionId = targetSessionId;
  if (!sessionId) {
    api.ui.toast({
      message: "No active session",
      variant: "warning"
    });
    return false;
  }
  const directory = api.state.path.directory ?? "";
  const detail = await loadEmbedDetail(sessionId, directory);
  if (getSessionId(api) !== sessionId) return false;
  api.ui.dialog.replace(() => _$createComponent(EmbedDialog, {
    api: api,
    detail: detail
  }));
  return true;
}
function showResultDialog(api, title, message) {
  api.ui.dialog.replace(() => _$createComponent(api.ui.DialogAlert, {
    title: title,
    message: message,
    onConfirm: () => {}
  }));
  return true;
}

/**
 * Register Magic Context command palette entries, preferring the v1.14.42+
 * `keymap.registerLayer` API and falling back to the legacy
 * `api.command.register` for older hosts.
 *
 * The `keymap.registerLayer` shape uses `name`/`title`/`run`/`namespace`
 * (see `@opencode-ai/plugin/tui` types) and is what the host's own legacy
 * command-shim translates into. Calling it directly skips the deprecation
 * warning and works without depending on the (now-deprecated) `api.command`
 * namespace existing at all.
 *
 * Version coverage:
 *   1.14.0–1.14.41 — `api.command.register` only
 *   1.14.42–1.14.43 — both surfaces broken (api.command removed, keymap landed
 *                     but with bugs); plugins crash on init either way
 *   1.14.44+        — `api.keymap.registerLayer` canonical, `api.command` shim
 */
function registerCommandPaletteEntries(api) {
  const apiAny = api;
  if (typeof apiAny.keymap?.registerLayer === "function") {
    // Audit Finding #2 hardening: even when registerLayer exists as a
    // function, the underlying keymap implementation in OpenCode TUI
    // 1.14.42-1.14.43 can throw at call time. Without the try-catch the
    // `return` below would propagate the throw and the legacy
    // `command.register` fallback path (~20 lines down) would be
    // unreachable. The cost is one debug log on the rare broken-TUI
    // build; the benefit is that older command.register-only TUIs
    // running alongside a partially-broken keymap surface still get
    // their command palette entries.
    try {
      apiAny.keymap.registerLayer({
        commands: [{
          namespace: "palette",
          name: "magic-context.status",
          title: "Magic Context: Status",
          category: "Magic Context",
          run() {
            showStatusDialog(api);
          }
        }, {
          namespace: "palette",
          name: "magic-context.recomp",
          title: "Magic Context: Recomp",
          category: "Magic Context",
          run() {
            showRecompDialog(api);
          }
        }],
        bindings: []
      });
      return;
    } catch (err) {
      console.debug("[magic-context-tui] keymap.registerLayer threw; falling back to command.register", err);
      // Fall through to legacy registration.
    }
  }
  if (typeof apiAny.command?.register === "function") {
    apiAny.command.register(() => [{
      title: "Magic Context: Status",
      value: "magic-context.status",
      category: "Magic Context",
      onSelect() {
        showStatusDialog(api);
      }
    }, {
      title: "Magic Context: Recomp",
      value: "magic-context.recomp",
      category: "Magic Context",
      onSelect() {
        showRecompDialog(api);
      }
    }]);
    return;
  }

  // Neither API surface is present. The TUI host can still load — we only
  // lose the command palette entry points. The sidebar (registered above
  // via api.slots.register) remains visible. Status/Recomp are still
  // reachable through the server-side `/ctx-status` and `/ctx-recomp`
  // slash commands, which the server handler bridges to the TUI dialogs
  // via RPC.
}

/**
 * Show the one-shot "What's new" dialog on TUI startup if the server tells us
 * to. The server is the source of truth: it has the version + features
 * constants AND owns the persistence file. We just render and report back.
 *
 * Failure-tolerant by design — if the server isn't ready or the RPC fails,
 * we silently skip (the next TUI launch will retry).
 */
/**
 * URLs render as plain text. Modern terminals (iTerm2, kitty, WezTerm, Ghostty,
 * recent macOS Terminal) auto-detect URLs and let users Cmd-click; older
 * terminals require manual copy. We tried opentui's `<a href>` JSX intrinsic
 * for application-level OSC 8 clickability, but it's a span-like element that
 * forced text out of opentui's word-wrap mode, causing bullets to bleed past
 * the dialog border. Pure-string children of `<text>` wrap correctly, so the
 * AFT-style DialogAlert + plain string is the right surface here.
 */
async function showStartupAnnouncement(api) {
  try {
    const ann = await getAnnouncement();
    if (!ann.show || !ann.version || !ann.features || ann.features.length === 0) return;
    const title = `Magic Context v${ann.version}`;
    const lines = ["What's new:", "", ...ann.features.map(line => `  • ${line}`)];
    if (ann.footer && ann.footer.trim().length > 0) {
      // Blank-line separator keeps the persistent footer (Discord invite,
      // etc.) visually distinct from the version-specific bullets.
      lines.push("", ann.footer);
    }
    const message = lines.join("\n");
    api.ui.dialog.replace(() => _$createComponent(api.ui.DialogAlert, {
      title: title,
      message: message,
      onConfirm: () => {
        void markAnnounced();
      }
    }), () => {
      // User dismissed via Escape rather than confirming. Mark
      // dismissed anyway — they saw the dialog, that's the contract.
      void markAnnounced();
    });
  } catch {
    // RPC not ready yet (port file missing or transient HTTP failure) —
    // silently skip. The next TUI start re-checks.
  }
}
const tui = async (api, _options, meta) => {
  // Initialize RPC client for server communication
  const directory = api.state.path.directory ?? "";
  initRpcClient(directory);
  await refreshToastDurationMs();

  // Register sidebar slot
  api.slots.register(createSidebarContentSlot(api));

  // Register TUI command palette entries (no slash field — slash commands
  // are registered server-side so there's only one /ctx-* registration).
  // The server detects TUI mode and sends dialog requests via RPC instead
  // of sendIgnoredMessage.
  //
  // OpenCode 1.14.42 removed `api.command.register` entirely
  // (anomalyco/opencode#26053). A later patch (1.14.44+) reinstated it as
  // a deprecated shim that translates to `api.keymap.registerLayer`. To
  // work across all hosts (1.14.0–1.14.41 with command-only, the broken
  // 1.14.42–1.14.43, and 1.14.44+ where both exist), we prefer
  // `api.keymap.registerLayer` and fall back to `api.command.register`
  // only when keymap is missing.
  registerCommandPaletteEntries(api);

  // Receive server→TUI notifications (toasts + dialog requests) over a single
  // persistent WebSocket, pushed the instant the server queues them. This
  // replaces the old 500ms HTTP poll whose new-connection-per-tick cost was the
  // source of idle TUI CPU (#200). The socket carries the active session in its
  // hello so the server scopes delivery; here we re-check the active session per
  // notification (it can change between queue and delivery) before acting.
  const handleNotification = async n => {
    const requestedSessionId = getSessionId(api);
    const generation = getRpcGeneration();
    // A session-scoped notification only applies while we're viewing that
    // session; global (session-less) ones always apply. Returning false leaves
    // it unacked so a TUI on the right session (or a later switch back) still
    // gets it.
    if (n.sessionId && requestedSessionId && n.sessionId !== requestedSessionId) {
      return false;
    }
    if (n.type === "toast") {
      const p = n.payload;
      showToast(api, {
        message: String(p.message ?? ""),
        variant: p.variant ?? "info",
        durationOverrideMs: typeof p.duration === "number" && Number.isFinite(p.duration) ? p.duration : undefined
      });
      return true;
    }
    if (n.type !== "action") return false;
    const action = n.payload?.action;
    const stillActive = () => getRpcGeneration() === generation && getSessionId(api) === requestedSessionId;
    if (action === "show-status-dialog") {
      return stillActive() && (await showStatusDialog(api, requestedSessionId));
    }
    if (action === "show-recomp-dialog") {
      return stillActive() && (await showRecompDialog(api, requestedSessionId));
    }
    if (action === "show-upgrade-dialog") {
      const resume = n.payload?.resume === true ? {
        stagedCount: Number(n.payload?.stagedCount ?? 0),
        stagedThrough: Number(n.payload?.stagedThrough ?? 0)
      } : undefined;
      return stillActive() && showUpgradeDialog(api, resume, requestedSessionId);
    }
    if (action === "show-embed-dialog") {
      return stillActive() && (await showEmbedDialog(api, requestedSessionId));
    }
    if (action === "wrapup-progress-kick") {
      // /ctx-wrapup blocks its command turn and fires no message events, so
      // the sidebar poll would never notice the run. Kick the fast progress
      // poll (same loop the recomp dialog kicks). The start toast arrives
      // separately via the ignored-message notification path.
      if (!stillActive()) return false;
      kickRecompProgressRefresh();
      return true;
    }
    if (action === "show-flush-dialog") {
      const flushMsg = String(n.payload?.message ?? "Flushed.");
      return stillActive() && showResultDialog(api, "Flush", flushMsg);
    }
    if (action === "show-result-dialog") {
      const title = String(n.payload?.title ?? "Magic Context");
      const body = String(n.payload?.message ?? "");
      return stillActive() && showResultDialog(api, title, body);
    }
    return false;
  };
  startNotificationSocket({
    getSessionId: () => getSessionId(api),
    onNotification: handleNotification
  });

  // Clean up on dispose
  api.lifecycle.onDispose(() => {
    stopNotificationSocket();
    closeRpc();
  });
  const conflictResult = detectConflicts(directory);
  if (conflictResult.hasConflict) {
    showConflictDialog(api, directory, conflictResult.reasons, conflictResult.conflicts);
    return;
  }

  // Show one-shot release announcement after conflict gate.
  // Fire-and-forget: if the server isn't ready or RPC fails, the next TUI
  // launch will retry. Dialog only appears once per ANNOUNCEMENT_VERSION
  // (persisted via mark-announced RPC writing last_announced_version).
  void showStartupAnnouncement(api);
};
const id = "opencode-magic-context";
export default {
  id,
  tui
};