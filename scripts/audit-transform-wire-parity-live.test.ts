import { describe, expect, test } from "bun:test";

import {
	evaluateOperatorTagTotalContract,
	operatorTagTotalFailureClasses,
} from "./audit-transform-wire-parity-live";

describe("hunt-9 live operator tag total contract", () => {
	test("requires the Rust status total to match stable module-store rows", () => {
		const matching = evaluateOperatorTagTotalContract(
			"rust",
			{ totalTags: 6 },
			{ totalTags: 6 },
			{ totalTags: 6, tagCountsAuthoritative: false },
		);
		const wrongSubset = evaluateOperatorTagTotalContract(
			"rust",
			{ totalTags: 6 },
			{ totalTags: 6 },
			{ totalTags: 2, tagCountsAuthoritative: false },
		);

		expect(matching.status_matches_direct_total).toBe(true);
		expect(matching.source_matches_lane).toBe(true);
		expect(wrongSubset.status_matches_direct_total).toBe(false);
		expect(operatorTagTotalFailureClasses("rust", matching)).toEqual([]);
		expect(operatorTagTotalFailureClasses("rust", wrongSubset)).toEqual([
			"rust_status_tag_total_mismatch",
		]);
	});

	test("rejects a context-mirror value as Rust tag authority", () => {
		const mirrored = evaluateOperatorTagTotalContract(
			"rust",
			{ totalTags: 6 },
			{ totalTags: 6 },
			{ totalTags: 6, tagCountsAuthoritative: true },
		);

		expect(mirrored.status_matches_direct_total).toBe(true);
		expect(mirrored.source_matches_lane).toBe(false);
		expect(mirrored.authority_source).toBe("host_or_unknown");
		expect(operatorTagTotalFailureClasses("rust", mirrored)).toEqual([
			"rust_status_tag_total_wrong_authority",
		]);
	});
});
