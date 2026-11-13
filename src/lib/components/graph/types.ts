import type { NodeReport } from '$lib/bridge';

/** Hidden target handle on every step node that world-model edges attach to. */
export const WORLD_HANDLE = '__world';

export interface StepNodeData extends Record<string, unknown> {
	node: NodeReport;
	selected: boolean;
	/** Mean ms per generated chunk for this revision, if it has run. */
	meanMs: number | null;
	onToggle: (id: string, enabled: boolean) => void;
	onRemove: (id: string) => void;
	onEditScript: (id: string) => void;
	onParam: (id: string, name: string, value: number) => void;
	onLabel: (id: string, label: string) => void;
}

export interface WorldNodeData extends Record<string, unknown> {
	model: string;
}
