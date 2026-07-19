import assert from "node:assert/strict";
import test from "node:test";
import { document, readInvocation, serializeDocument } from "../src/index.js";

test("reads only host-authored invocation variables", () => {
  assert.deepEqual(
    readInvocation({
      NAGI_PLUGIN_ID: "example.ci",
      NAGI_PLUGIN_ACTION_ID: "inspect",
      NAGI_PLUGIN_CONTEXT_JSON: '{"workspace_id":"w1"}',
    }),
    { pluginId: "example.ci", actionId: "inspect", context: { workspace_id: "w1" } },
  );
});

test("serializes a bounded host-compatible document", () => {
  const value = document([{ type: "notice", tone: "success", body: "All green" }], "CI");
  assert.equal(JSON.parse(serializeDocument(value)).schema_version, 1);
});

test("rejects control characters", () => {
  assert.throws(() => document([{ type: "notice", tone: "danger", body: "bad\u0007" }]));
});

