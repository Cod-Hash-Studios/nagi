export const CONTRACT_VERSION = 1;

export function readInvocation(env = process.env) {
  if (!env.NAGI_PLUGIN_ID) throw new Error("NAGI_PLUGIN_ID is missing");
  return {
    pluginId: env.NAGI_PLUGIN_ID,
    actionId: env.NAGI_PLUGIN_ACTION_ID || null,
    context: JSON.parse(env.NAGI_PLUGIN_CONTEXT_JSON || "{}"),
  };
}

export function document(blocks, summary = null) {
  const value = { schema_version: CONTRACT_VERSION, blocks };
  if (summary !== null) value.summary = summary;
  validateDocument(value);
  return value;
}

export function validateDocument(value) {
  if (value.schema_version !== CONTRACT_VERSION) throw new Error("unsupported UI document schema version");
  if (!Array.isArray(value.blocks) || value.blocks.length > 32) throw new Error("UI document exceeds 32 blocks");
  if (value.summary != null) bounded(value.summary, 1, 160, "summary");
  for (const block of value.blocks) {
    if (!block || !["section", "metrics", "list", "notice"].includes(block.type)) throw new Error("unknown UI block type");
    if (block.type === "notice") bounded(block.body, 1, 2048, "notice body");
    if (block.type === "section" && (!Array.isArray(block.rows) || block.rows.length > 64)) throw new Error("section exceeds 64 rows");
    if (block.type === "metrics" && (!Array.isArray(block.items) || block.items.length > 16)) throw new Error("metrics block exceeds 16 items");
    if (block.type === "list" && (!Array.isArray(block.items) || block.items.length > 64)) throw new Error("list exceeds 64 items");
  }
  return value;
}

export function serializeDocument(value) {
  validateDocument(value);
  return `${JSON.stringify(value)}\n`;
}

function bounded(value, min, max, label) {
  if (typeof value !== "string" || value.length < min || value.length > max || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new Error(`${label} must contain ${min}..=${max} safe characters`);
  }
}

