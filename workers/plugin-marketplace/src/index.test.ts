import { describe, expect, test } from "bun:test";
import worker, { normalizeRepositories, refreshPlugins, type Env } from "./index";

class MemoryR2 {
  objects = new Map<string, { value: string; options?: unknown }>();

  async get(key: string): Promise<{ text(): Promise<string> } | null> {
    const object = this.objects.get(key);
    return object ? { async text() { return object.value; } } : null;
  }

  async put(key: string, value: string, options?: unknown): Promise<void> {
    this.objects.set(key, { value, options });
  }
}

class MemoryKV {
  constructor(private readonly names: string[]) {}

  async list(options?: { prefix?: string }): Promise<{ keys: Array<{ name: string }> }> {
    return {
      keys: this.names
        .filter((name) => !options?.prefix || name.startsWith(options.prefix))
        .map((name) => ({ name })),
    };
  }
}

const sha = "a".repeat(40);
const manifest = `manifest_version = 2
id = "example.review"
name = "Review"
version = "1.2.3"
min_nagi_version = "0.7.4"
runtime = "wasi-component"
entrypoint = "dist/review.wasm"
capabilities = ["mission.read"]
`;
const component = new Uint8Array([0, 97, 115, 109, 13, 0, 1, 0]);

function repo(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: 1,
    full_name: "Cod-Hash-Studios/nagi-plugin-example",
    owner: { login: "Cod-Hash-Studios" },
    name: "nagi-plugin-example",
    description: "Example plugin",
    html_url: "https://github.com/Cod-Hash-Studios/nagi-plugin-example",
    stargazers_count: 5,
    language: "Rust",
    default_branch: "main",
    pushed_at: "2026-06-03T00:00:00Z",
    archived: false,
    fork: false,
    disabled: false,
    private: false,
    ...overrides,
  };
}

function environment(bucket = new MemoryR2(), blacklist?: MemoryKV): Env {
  return {
    PLUGIN_MARKETPLACE_BUCKET: bucket,
    PLUGIN_MARKETPLACE_BLACKLIST: blacklist,
    GITHUB_TOKEN: "token",
  };
}

function verifiedFetch(options: { component?: Uint8Array; repo?: Record<string, unknown> } = {}) {
  const calls: string[] = [];
  const fetch = async (input: RequestInfo | URL): Promise<Response> => {
    const url = new URL(input.toString());
    calls.push(url.toString());
    if (url.pathname === "/search/repositories") {
      return Response.json({ total_count: 1, items: [options.repo ?? repo()] });
    }
    if (url.hostname === "api.github.com" && url.pathname.includes("/commits/")) {
      return Response.json({ sha });
    }
    if (url.pathname.endsWith("/nagi-plugin.toml")) return new Response(manifest);
    if (url.pathname.endsWith("/dist/review.wasm")) {
      return new Response(options.component ?? component);
    }
    return new Response("not found", { status: 404 });
  };
  return { fetch: fetch as typeof globalThis.fetch, calls };
}

describe("normalizeRepositories", () => {
  test("normalizes only safe public repositories without assigning trust", () => {
    const plugins = normalizeRepositories([
      repo(),
      repo({ archived: true }),
      repo({ fork: true }),
      repo({ private: true }),
      repo({ html_url: "https://example.com/not-github" }),
    ]);

    expect(plugins).toHaveLength(1);
    expect(plugins[0]).toEqual({
      id: 1,
      fullName: "Cod-Hash-Studios/nagi-plugin-example",
      owner: "Cod-Hash-Studios",
      name: "nagi-plugin-example",
      description: "Example plugin",
      url: "https://github.com/Cod-Hash-Studios/nagi-plugin-example",
      stars: 5,
      language: "Rust",
      defaultBranch: "main",
      pushedAt: "2026-06-03T00:00:00Z",
    });
  });
});

describe("refreshPlugins", () => {
  test("pins a commit, parses manifest v2, scans bytes, and writes provenance", async () => {
    const bucket = new MemoryR2();
    const mock = verifiedFetch();
    const result = await refreshPlugins(environment(bucket), {
      fetch: mock.fetch,
      now: new Date("2026-06-20T12:00:00.000Z"),
      logger: { error() {} },
    });

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(mock.calls.some((url) => url.includes(`/commits/main`))).toBe(true);
    expect(mock.calls.some((url) => url.includes(`/${sha}/nagi-plugin.toml`))).toBe(true);
    expect(mock.calls.some((url) => url.includes(`/${sha}/dist/review.wasm`))).toBe(true);
    expect(result.snapshot.source).toMatchObject({ collectedCount: 1, verifiedCount: 1 });
    expect(result.snapshot.plugins[0]).toMatchObject({
      id: "example.review",
      version: "1.2.3",
      runtime: "wasi-component",
      capabilities: ["mission.read"],
      reviewStatus: "official",
      source: { commit: sha, repository: "Cod-Hash-Studios/nagi-plugin-example" },
      scans: { verdict: "pass" },
      capabilityDiff: { added: ["mission.read"], removed: [] },
    });
    expect(result.snapshot.plugins[0].artifact.sha256).toHaveLength(64);
    expect(bucket.objects.get("plugins/index.json")?.options).toEqual({
      httpMetadata: {
        contentType: "application/json; charset=utf-8",
        cacheControl: "public, max-age=300, s-maxage=1800, stale-while-revalidate=3600",
      },
    });
  });

  test("fails a candidate closed without publishing its unsafe artifact", async () => {
    const result = await refreshPlugins(environment(), {
      fetch: verifiedFetch({ component: new Uint8Array([1, 2, 3]) }).fetch,
      logger: { error() {} },
    });

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.snapshot.plugins).toEqual([]);
    expect(result.snapshot.source.warnings?.[0]).toContain("automated scans failed");
  });

  test("honors the emergency repository kill switch", async () => {
    const result = await refreshPlugins(
      environment(new MemoryR2(), new MemoryKV(["repo:cod-hash-studios/nagi-plugin-example"])),
      { fetch: verifiedFetch().fetch, logger: { error() {} } },
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.snapshot.plugins).toEqual([]);
  });

  test("does not overwrite the last good snapshot when discovery fails", async () => {
    const bucket = new MemoryR2();
    await bucket.put("plugins/index.json", '{"schemaVersion":2,"plugins":[{"id":"safe"}]}');
    const result = await refreshPlugins(environment(bucket), {
      fetch: (async () => new Response("rate limited", { status: 429 })) as typeof fetch,
      logger: { error() {} },
    });

    expect(result.ok).toBe(false);
    expect(bucket.objects.get("plugins/index.json")?.value).toContain('"safe"');
  });
});

describe("fetch handler", () => {
  test("serves only the versioned public snapshot route", async () => {
    const bucket = new MemoryR2();
    await bucket.put("plugins/index.json", '{"schemaVersion":2,"plugins":[]}');
    const env = environment(bucket);
    const listed = await worker.fetch(new Request("https://plugins.example/v1/plugins"), env);
    const missing = await worker.fetch(new Request("https://plugins.example/anything"), env);

    expect(listed.status).toBe(200);
    expect(listed.headers.get("Access-Control-Allow-Origin")).toBe("*");
    expect(missing.status).toBe(404);
  });
});
