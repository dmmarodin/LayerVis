import type { Frame, NodeReport } from '$lib/bridge';

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

export interface FrameNodeData extends Record<string, unknown> {
	frame: Frame;
	onLabel: (id: string, label: string) => void;
	/** Final geometry after a resize (position changes when dragging top/left edges). */
	onResize: (id: string, x: number, y: number, width: number, height: number) => void;
}

export interface RerouteNodeData extends Record<string, unknown> {
	node: NodeReport;
	selected: boolean;
}
