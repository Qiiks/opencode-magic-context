import { readFileSync } from "node:fs";

/**
 * Shared resolver for pi-coding-agent's session APIs, used by every Pi dreamer
 * provider that reads historical JSONL sessions (retrospective, refresh-primers).
 *
 * ONE resolver on purpose: the session-listing API drifted once already
 * (`SessionManager.listSessions` never existed publicly; listing is
 * `SessionManager.listAll`, and `loadEntriesFromFile` is not exported — entries
 * come from `readFileSync` + `parseSessionEntries`). When each provider carried
 * its own copy of this lookup, one copy got fixed and the other kept probing the
 * nonexistent API, so its feature silently degraded. Any future Pi API drift
 * should break exactly one resolver and one test.
 */
export interface PiSessionApi {
	listSessions: (sessionDir?: string) => unknown[] | Promise<unknown[]>;
	loadEntriesFromFile: (filePath: string) => unknown[] | Promise<unknown[]>;
}

const PI_CODING_AGENT_MODULE = "@earendil-works/pi-coding-agent";

export async function loadDefaultPiSessionApi(): Promise<PiSessionApi> {
	const mod = (await import(/* @vite-ignore */ PI_CODING_AGENT_MODULE)) as {
		SessionManager?: {
			listAll?: (sessionDir?: string) => unknown[] | Promise<unknown[]>;
		};
		loadEntriesFromFile?: (filePath: string) => unknown[] | Promise<unknown[]>;
		parseSessionEntries?: (content: string) => unknown[];
	};
	const listSessions = mod.SessionManager?.listAll;
	if (typeof listSessions !== "function") {
		throw new Error(
			"Pi session APIs unavailable: expected SessionManager.listAll on pi-coding-agent",
		);
	}
	// loadEntriesFromFile is NOT part of pi-coding-agent's public API — fall back
	// to readFileSync + parseSessionEntries (both exported).
	const loadEntriesFromFile: PiSessionApi["loadEntriesFromFile"] =
		mod.loadEntriesFromFile ??
		((filePath: string) => {
			const content = readFileSync(filePath, "utf8");
			return mod.parseSessionEntries?.(content) ?? [];
		});
	return {
		listSessions: listSessions.bind(mod.SessionManager),
		loadEntriesFromFile,
	};
}
