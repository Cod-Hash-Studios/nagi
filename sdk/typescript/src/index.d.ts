export type Tone = "neutral" | "success" | "warning" | "danger";
export type UiRow = { label: string; value: string; tone?: Tone };
export type UiMetric = { label: string; value: string; detail?: string; tone?: Tone };
export type UiListItem = { title: string; detail?: string; tone?: Tone };
export type UiBlock =
  | { type: "section"; title: string; rows: UiRow[] }
  | { type: "metrics"; items: UiMetric[] }
  | { type: "list"; title?: string; items: UiListItem[] }
  | { type: "notice"; tone: Tone; title?: string; body: string };
export interface UiDocument { schema_version: 1; summary?: string; blocks: UiBlock[] }
export interface Invocation { pluginId: string; actionId: string | null; context: unknown }
export declare const CONTRACT_VERSION: 1;
export declare function readInvocation(env?: Record<string, string | undefined>): Invocation;
export declare function document(blocks: UiBlock[], summary?: string | null): UiDocument;
export declare function validateDocument(value: UiDocument): UiDocument;
export declare function serializeDocument(value: UiDocument): string;

