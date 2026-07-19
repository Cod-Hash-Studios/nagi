import { describe, expect, test } from "bun:test";
import { parsePluginManifest } from "./manifest";

const valid = `
manifest_version = 2
id = "example.review"
name = "Review"
version = "1.2.3"
min_nagi_version = "0.7.4"
runtime = "wasi-component"
entrypoint = "dist/review.wasm"
capabilities = ["mission.read", "workspace.files.read:worktree"]
`;

describe("parsePluginManifest", () => {
  test("parses the bounded marketplace contract", () => {
    expect(parsePluginManifest(valid)).toEqual({
      manifestVersion: 2,
      id: "example.review",
      name: "Review",
      version: "1.2.3",
      minNagiVersion: "0.7.4",
      runtime: "wasi-component",
      entrypoint: "dist/review.wasm",
      capabilities: ["mission.read", "workspace.files.read:worktree"],
    });
  });

  test("rejects native runtimes and path escapes", () => {
    expect(() => parsePluginManifest(valid.replace("wasi-component", "trusted-native"))).toThrow(
      "sandboxed",
    );
    expect(() => parsePluginManifest(valid.replace("dist/review.wasm", "../review.wasm"))).toThrow(
      "entrypoint",
    );
  });

  test("rejects duplicate capabilities and oversized input", () => {
    expect(() =>
      parsePluginManifest(valid.replace(
        '["mission.read", "workspace.files.read:worktree"]',
        '["mission.read", "mission.read"]',
      )),
    ).toThrow("duplicate");
    expect(() => parsePluginManifest(`${valid}\n${"#".repeat(70_000)}`)).toThrow("65536");
  });
});
