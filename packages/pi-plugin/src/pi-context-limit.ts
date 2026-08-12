import {
	isSaneLimit,
	resolveLimit,
} from "@magic-context/core/shared/models-dev-cache";

export interface PiModelLimit {
	provider?: string;
	id?: string;
	contextWindow?: number;
	maxTokens?: number;
}

/** Resolve Pi's raw runtime window through the shared output-reservation chokepoint. */
export function resolvePiUsableContextLimit(args: {
	rawContextWindow: number | undefined;
	model?: PiModelLimit;
	detectedContextLimit?: number;
	/** Persisted scheduler input tokens used to reconstruct its usable limit when Pi omits maxTokens. */
	persistedInputTokens?: number;
	/** Persisted scheduler percentage paired with persistedInputTokens. */
	persistedPercentage?: number;
}): number | undefined {
	const rawContext = isSaneLimit(args.rawContextWindow)
		? args.rawContextWindow
		: undefined;
	const detected =
		typeof args.detectedContextLimit === "number" &&
		Number.isFinite(args.detectedContextLimit) &&
		args.detectedContextLimit >= 1024
			? args.detectedContextLimit
			: undefined;
	const context =
		rawContext !== undefined && detected !== undefined
			? Math.min(rawContext, detected)
			: (rawContext ?? detected);
	if (context === undefined) return undefined;

	const usableLimit = resolveLimit(
		{ context, output: args.model?.maxTokens },
		args.model?.provider ?? "unknown",
		args.model?.id ?? "unknown",
	);
	const hasOutputBudget =
		typeof args.model?.maxTokens === "number" &&
		Number.isFinite(args.model.maxTokens) &&
		args.model.maxTokens >= 0;
	if (hasOutputBudget) return usableLimit;

	const persistedLimit = resolvePersistedPiContextLimit(args);
	if (persistedLimit === undefined) return usableLimit;
	// Do not let a larger persisted limit override a smaller runtime or overflow cap.
	return usableLimit === undefined
		? persistedLimit
		: Math.min(persistedLimit, usableLimit);
}

function resolvePersistedPiContextLimit(args: {
	persistedInputTokens?: number;
	persistedPercentage?: number;
}): number | undefined {
	if (
		typeof args.persistedInputTokens !== "number" ||
		!Number.isFinite(args.persistedInputTokens) ||
		args.persistedInputTokens <= 0 ||
		typeof args.persistedPercentage !== "number" ||
		!Number.isFinite(args.persistedPercentage) ||
		args.persistedPercentage <= 0
	) {
		return undefined;
	}

	const limit = Math.round(
		args.persistedInputTokens / (args.persistedPercentage / 100),
	);
	return Number.isFinite(limit) && limit >= 1024 ? limit : undefined;
}
