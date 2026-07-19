const MAX_MANIFEST_BYTES = 64 * 1024;
const SAFE_ID = /^[A-Za-z0-9][A-Za-z0-9:._-]{0,119}$/;
const SAFE_VERSION = /^[A-Za-z0-9][A-Za-z0-9.+_-]{0,127}$/;

export type MarketplaceManifest = {
  manifestVersion: 2;
  id: string;
  name: string;
  version: string;
  minNagiVersion: string;
  runtime: "wasi-component";
  entrypoint: string;
  capabilities: string[];
};

export function parsePluginManifest(text: string): MarketplaceManifest {
  if (new TextEncoder().encode(text).byteLength > MAX_MANIFEST_BYTES) {
    throw new Error(`plugin manifest exceeds ${MAX_MANIFEST_BYTES} bytes`);
  }
  const values = topLevelAssignments(text);
  const manifestVersion = requiredInteger(values, "manifest_version");
  if (manifestVersion !== 2) throw new Error("marketplace requires manifest_version = 2");

  const id = requiredString(values, "id");
  if (!SAFE_ID.test(id)) throw new Error("plugin id is invalid");
  const name = requiredString(values, "name");
  if (name.length > 160 || hasUnsafeControl(name)) throw new Error("plugin name is invalid");
  const version = requiredString(values, "version");
  const minNagiVersion = requiredString(values, "min_nagi_version");
  if (!SAFE_VERSION.test(version) || !SAFE_VERSION.test(minNagiVersion)) {
    throw new Error("plugin version is invalid");
  }
  const runtime = requiredString(values, "runtime");
  if (runtime !== "wasi-component") {
    throw new Error("marketplace plugins must use the sandboxed wasi-component runtime");
  }
  const entrypoint = requiredString(values, "entrypoint");
  validateEntrypoint(entrypoint);
  const capabilities = optionalStringArray(values, "capabilities");
  const unique = new Set(capabilities);
  if (unique.size !== capabilities.length) throw new Error("plugin capabilities contain duplicates");
  if (capabilities.length > 64 || capabilities.some((value) => value.length > 2_048 || hasUnsafeControl(value))) {
    throw new Error("plugin capabilities are invalid");
  }

  return {
    manifestVersion: 2,
    id,
    name,
    version,
    minNagiVersion,
    runtime,
    entrypoint,
    capabilities,
  };
}

function topLevelAssignments(text: string): Map<string, string> {
  const values = new Map<string, string>();
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    if (line.startsWith("[")) break;
    const match = /^([A-Za-z0-9_]+)\s*=\s*(.+)$/.exec(line);
    if (!match) throw new Error("plugin manifest contains unsupported top-level syntax");
    if (values.has(match[1])) throw new Error(`plugin manifest repeats ${match[1]}`);
    values.set(match[1], match[2].trim());
  }
  return values;
}

function requiredString(values: Map<string, string>, key: string): string {
  const raw = values.get(key);
  if (!raw) throw new Error(`plugin manifest is missing ${key}`);
  try {
    const value: unknown = JSON.parse(raw);
    if (typeof value !== "string" || value.length === 0) throw new Error();
    return value;
  } catch {
    throw new Error(`plugin manifest ${key} must be a quoted string`);
  }
}

function requiredInteger(values: Map<string, string>, key: string): number {
  const raw = values.get(key);
  if (!raw || !/^\d+$/.test(raw)) throw new Error(`plugin manifest ${key} must be an integer`);
  return Number(raw);
}

function optionalStringArray(values: Map<string, string>, key: string): string[] {
  const raw = values.get(key);
  if (!raw) return [];
  try {
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value) || value.some((entry) => typeof entry !== "string" || entry.length === 0)) {
      throw new Error();
    }
    return value as string[];
  } catch {
    throw new Error(`plugin manifest ${key} must be an array of quoted strings`);
  }
}

function validateEntrypoint(value: string): void {
  const segments = value.split("/");
  if (
    value.length > 4_096 ||
    value.startsWith("/") ||
    value.startsWith("\\") ||
    /^[A-Za-z]:/.test(value) ||
    segments.some((segment) => !segment || segment === "." || segment === "..") ||
    !value.endsWith(".wasm") ||
    hasUnsafeControl(value)
  ) {
    throw new Error("plugin entrypoint must be a safe relative .wasm path");
  }
}

function hasUnsafeControl(value: string): boolean {
  return [...value].some((character) => character.charCodeAt(0) < 32 || character.charCodeAt(0) === 127);
}
