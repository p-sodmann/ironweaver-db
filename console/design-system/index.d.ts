import type * as React from 'react';
export type Action = 'expand' | 'connect' | 'detach' | 'filter' | 'focus' | 'hide' | 'wander' | 'run' | 'save';
export type RunState = 'idle' | 'running' | 'done';
export interface GNode { id: string; label: string; caption: string; x: number; y: number; expanded?: boolean }
export interface GEdge { id: string; s: string; t: string; type: string }
export interface WorkbenchProps {}
export declare function Workbench(props: WorkbenchProps): React.ReactElement;
export interface StatusRailProps { db?: string; metrics?: { nodes: number; rels: number; heap: number; heapMax: number; cache: number; p50: number }; tx?: number; txOpen?: number; activity?: boolean; health?: 'ok' | 'warn' | 'danger'; open?: boolean; onToggle?: (open: boolean) => void; onCommand?: () => void; onHelp?: () => void }
export declare function StatusRail(props: StatusRailProps): React.ReactElement;
export interface SchemaItem { id: string; name: string; label?: string; rel?: boolean; count?: number; meta?: string; state?: 'ONLINE' | 'POPULATING' | 'FAILED'; pct?: number; glyph?: 'db'; active?: boolean }
export interface SchemaNavigatorProps { sections?: { id: string; title: string; collapsed?: boolean; items: SchemaItem[] }[]; selected?: string; onSelect?: (item: SchemaItem) => void; onActivate?: (item: SchemaItem) => void }
export declare function SchemaNavigator(props: SchemaNavigatorProps): React.ReactElement;
export interface GraphCanvasProps { graph?: { nodes: GNode[]; edges: GEdge[] }; selected?: string | null; onSelect?: (node: GNode | null) => void; onLog?: (level: 'INFO' | 'WARN' | 'ERROR', msg: string) => void; onExpandRef?: React.MutableRefObject<((id: string) => void) | null>; empty?: boolean }
export declare function GraphCanvas(props: GraphCanvasProps): React.ReactElement;
export interface InspectorProps { node?: { id: string; label: string; caption: string }; onSave?: (node: any, key: string, value: string) => void; onExpand?: (id: string) => void; onClose?: () => void }
export declare function Inspector(props: InspectorProps): React.ReactElement;
export interface QueryConsoleProps { query?: string; onRun?: (text: string) => number | Promise<number | void> | void; open?: boolean; onOpenChange?: (open: boolean) => void }
export declare function QueryConsole(props: QueryConsoleProps): React.ReactElement;
export interface RunCapProps { state?: RunState; onRun?: () => void; ms?: string | number; height?: number }
export declare function RunCap(props: RunCapProps): React.ReactElement;
export interface RadialMenuProps { x?: number; y?: number; nodeRadius?: number; items?: { id: string; label: string; key: string }[]; onPick?: (id: string) => void; onClose?: () => void; standalone?: boolean }
export declare function RadialMenu(props: RadialMenuProps): React.ReactElement;
export interface NavPuckProps { nodes?: { id: string; x: number; y: number }[]; view?: { x: number; y: number; k: number }; size?: { w: number; h: number }; onPan?: (p: { x: number; y: number }) => void; onZoom?: (factor: number) => void; onFit?: () => void }
export declare function NavPuck(props: NavPuckProps): React.ReactElement;
export interface ResultTableProps { columns?: { key: string; name: string; type: string; num?: boolean }[]; rows?: { id: string; cells: ({ v: string } | { node: { id: string; label: string; caption: string } })[] }[]; selected?: string; onSelect?: (id: string) => void; morph?: boolean; footer?: string }
export declare function ResultTable(props: ResultTableProps): React.ReactElement;
export interface PlanViewProps { plan?: { id: number; depth: number; op: string; detail: string; est: number; rows: number; hits: number; ms: number; hot?: boolean; warn?: string }[] }
export declare function PlanView(props: PlanViewProps): React.ReactElement;
export interface LogStreamProps { entries?: { t: string; level: 'INFO' | 'WARN' | 'ERROR'; msg: string; fresh?: boolean }[]; follow?: boolean }
export declare function LogStream(props: LogStreamProps): React.ReactElement;
export interface GlyphProps { action: Action; state?: RunState; play?: boolean; size?: number; title?: string }
export declare function Glyph(props: GlyphProps): React.ReactElement;
export interface ButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> { variant?: 'default' | 'primary' | 'signal' | 'ghost'; glyph?: Action; glyphState?: RunState; play?: boolean; kbd?: string; showKey?: boolean; active?: boolean; iconOnly?: boolean }
export declare function Button(props: ButtonProps): React.ReactElement;
export interface LogTickerProps { entries?: LogStreamProps['entries']; open?: boolean; onOpen?: () => void }
export declare function LogTicker(props: LogTickerProps): React.ReactElement;
export interface ShortcutSheetProps { onClose?: () => void }
export declare function ShortcutSheet(props: ShortcutSheetProps): React.ReactElement;
export interface ViewSwitchProps { items?: { id: string; label: string; key: string }[]; value: string; onChange?: (id: string) => void }
export declare function ViewSwitch(props: ViewSwitchProps): React.ReactElement;
export interface SaveAckProps { tx: number; label?: string }
export declare function SaveAck(props: SaveAckProps): React.ReactElement;
export interface EmptyStateProps { kind?: 'canvas' | 'results' | 'log' | 'selection'; title?: string; body?: string }
export declare function EmptyState(props: EmptyStateProps): React.ReactElement;
export interface KeyProps { children: React.ReactNode; dim?: boolean; hint?: boolean }
export declare function Key(props: KeyProps): React.ReactElement;
export interface TagProps { label?: string; rel?: boolean; count?: number; children?: React.ReactNode }
export declare function Tag(props: TagProps): React.ReactElement;
export interface LedProps { state?: 'ok' | 'warn' | 'danger' | 'off'; activity?: boolean; children?: React.ReactNode }
export declare function Led(props: LedProps): React.ReactElement;
export interface MeterProps { value: number; max?: number; warnAt?: number; caption?: string; readout?: string; width?: number }
export declare function Meter(props: MeterProps): React.ReactElement;
export interface NodeShapeProps extends React.SVGAttributes<SVGElement> { label?: string; shape?: 'circle' | 'diamond' | 'square'; color?: string; r?: number }
export declare function NodeShape(props: NodeShapeProps): React.ReactElement;
export interface ShapeGlyphProps { label?: string; shape?: 'circle' | 'diamond' | 'square'; color?: string; size?: number }
export declare function ShapeGlyph(props: ShapeGlyphProps): React.ReactElement;
declare global { interface Window { IronWeaver: { Workbench: typeof Workbench; StatusRail: typeof StatusRail; SchemaNavigator: typeof SchemaNavigator; GraphCanvas: typeof GraphCanvas; Inspector: typeof Inspector; QueryConsole: typeof QueryConsole; RunCap: typeof RunCap; RadialMenu: typeof RadialMenu; NavPuck: typeof NavPuck; ResultTable: typeof ResultTable; PlanView: typeof PlanView; LogStream: typeof LogStream; LogTicker: typeof LogTicker; ShortcutSheet: typeof ShortcutSheet; Glyph: typeof Glyph; Button: typeof Button; ViewSwitch: typeof ViewSwitch; SaveAck: typeof SaveAck; EmptyState: typeof EmptyState; Key: typeof Key; Tag: typeof Tag; Led: typeof Led; Meter: typeof Meter; NodeShape: typeof NodeShape; ShapeGlyph: typeof ShapeGlyph; demo: any } } }
