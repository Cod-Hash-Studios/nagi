import { describe, expect, test } from "bun:test";
import { capabilityDiff, scanPluginArtifact } from "./scans";

describe("scanPluginArtifact", () => {
  test("accepts a bounded WebAssembly component without leaked secrets", async () => {
    const result = await scanPluginArtifact({
      manifestText: 'id = "example.safe"',
      component: new Uint8Array([0, 97, 115, 109, 13, 0, 1, 0]),
    });

    expect(result.verdict).toBe("pass");
    expect(result.checks.map((check) => check.id)).toEqual([
      "component-format",
      "artifact-size",
      "secret-patterns",
    ]);
  });

  test("fails closed on invalid bytes and secret-like metadata", async () => {
    const result = await scanPluginArtifact({
      manifestText: 'token = "ghp_abcdefghijklmnopqrstuvwxyz1234567890"',
      component: new Uint8Array([1, 2, 3]),
    });

    expect(result.verdict).toBe("fail");
    expect(result.checks.filter((check) => check.status === "fail").length).toBe(2);
  });
});

describe("capabilityDiff", () => {
  test("shows added and removed authority without using stars as trust", () => {
    expect(capabilityDiff(["mission.read", "workspace.files.write:worktree"], ["mission.read"]))
      .toEqual({ added: ["workspace.files.write:worktree"], removed: [] });
  });
});
