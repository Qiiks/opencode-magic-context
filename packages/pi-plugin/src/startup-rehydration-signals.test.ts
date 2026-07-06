import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

describe("Pi startup rehydration signal contract", () => {
	it("rehydrates pending Pi marker sessions into history and materialization signals", () => {
		const source = readFileSync(join(import.meta.dir, "index.ts"), "utf8");
		const block = source.slice(
			source.indexOf("const pendingPiMarkerSessions"),
			source.indexOf(
				"Magic Context (pi) failed to rehydrate deferred Pi compaction markers",
			),
		);
		expect(block).toContain("signalPiDeferredHistoryRefresh(sid)");
		expect(block).toContain("signalPiDeferredMaterialization(sid)");
		expect(block).not.toContain("signalPiPendingMaterialization(sid)");
	});
});
