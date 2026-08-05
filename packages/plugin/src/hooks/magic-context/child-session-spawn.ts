import {
    type ChildSpawnFenceFailure,
    probeChildSpawnFence,
} from "../../features/magic-context/schema-fence-probe";
import { updateSessionMeta } from "../../features/magic-context/storage";
import { sessionLog } from "../../shared/logger";
import { pushNotification } from "../../shared/rpc-notifications";
import type { Database } from "../../shared/sqlite";
import { type NotificationParams, sendIgnoredMessage } from "./send-session-notification";

export const STALE_PLUGIN_RESTART_NOTICE =
    "Magic Context: plugin build is older than its database — restart OpenCode";

interface ChildSessionClient {
    session: { create(input: never): unknown | Promise<unknown> };
}

interface ChildSessionSpawnArgs {
    client: ChildSessionClient;
    db: Database | null;
    parentSessionId?: string;
    title: string;
    directory?: string;
    notificationParams?: NotificationParams;
    /** Test seam for the one-shot, N-consecutive failure surface. */
    onFenceLatched?: (failure: ChildSpawnFenceFailure) => void | Promise<void>;
}

async function surfaceStalePluginBuild(args: ChildSessionSpawnArgs): Promise<void> {
    if (!args.parentSessionId) return;
    try {
        if (args.db) {
            updateSessionMeta(args.db, args.parentSessionId, {
                lastTransformError: STALE_PLUGIN_RESTART_NOTICE,
            });
        }
        pushNotification(
            "toast",
            {
                title: "Magic Context",
                message: STALE_PLUGIN_RESTART_NOTICE,
                variant: "error",
                duration: 10_000,
            },
            args.parentSessionId,
        );
        // The toast's companion action makes the persisted sidebar error visible
        // immediately instead of waiting for the next session event or poll.
        pushNotification("action", { action: "refresh-sidebar" }, args.parentSessionId);
        // This is the same out-of-band boot-warning surface used for #266 mode
        // transitions. It never joins the transform message array or nudge path.
        await sendIgnoredMessage(
            args.client,
            args.parentSessionId,
            STALE_PLUGIN_RESTART_NOTICE,
            args.notificationParams ?? {},
            true,
        );
    } catch (error) {
        sessionLog(
            args.parentSessionId,
            `stale plugin-build warning delivery failed: ${error instanceof Error ? error.message : String(error)}`,
        );
    }
}

/**
 * Shared OpenCode child-session choke point. Every historian/recomp, dreamer,
 * and sidekick child must pass this probe before asking OpenCode to create it.
 */
export async function createChildSessionWithFence(
    args: ChildSessionSpawnArgs,
): Promise<unknown | null> {
    const verdict = probeChildSpawnFence(args.db);
    if (!verdict.allowSpawn) {
        if (args.parentSessionId) {
            sessionLog(
                args.parentSessionId,
                `child session skipped (${verdict.failure.failureClass}): database=v${verdict.failure.persistedVersion}, supported_fence=v${verdict.failure.supportedVersion}, consecutive=${verdict.failure.consecutiveFailures}, total=${verdict.failure.totalFailures}`,
            );
        }
        if (verdict.shouldSurface) {
            if (args.onFenceLatched) await args.onFenceLatched(verdict.failure);
            else await surfaceStalePluginBuild(args);
        }
        return null;
    }

    return args.client.session.create({
        body: {
            ...(args.parentSessionId ? { parentID: args.parentSessionId } : {}),
            title: args.title,
        },
        query: { directory: args.directory },
    } as never);
}
