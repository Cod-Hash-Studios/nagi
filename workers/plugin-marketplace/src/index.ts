import { parsePluginManifest, type MarketplaceManifest } from "./manifest";
import { capabilityDiff, scanPluginArtifact, sha256, type ScanResult } from "./scans";
import { readSubmissionCandidates, type SubmissionNamespace } from "./submissions";

const SNAPSHOT_KEY = "plugins/index.json";
const SNAPSHOT_CACHE_CONTROL = "public, max-age=300, s-maxage=1800, stale-while-revalidate=3600";
const GITHUB_QUERY = "topic:nagi-plugin is:public";
const GITHUB_API_VERSION = "2022-11-28";
const GITHUB_SEARCH_URL = "https://api.github.com/search/repositories";
const BLACKLIST_REPO_KEY_PREFIX = "repo:";
const PER_PAGE = 100;
const MAX_REPOS = 1000;
const MAX_MANIFEST_BYTES = 64 * 1024;
const MAX_COMPONENT_BYTES = 16 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 10_000;

type R2Object = { text(): Promise<string> };
type R2Bucket = {
  get(key: string): Promise<R2Object | null>;
  put(
    key: string,
    value: string,
    options?: { httpMetadata?: { contentType?: string; cacheControl?: string } },
  ): Promise<unknown>;
};

type KVNamespace = SubmissionNamespace;
type ExecutionContext = { waitUntil(promise: Promise<unknown>): void };
type ScheduledController = unknown;

export type Env = {
  PLUGIN_MARKETPLACE_BUCKET: R2Bucket;
  PLUGIN_MARKETPLACE_BLACKLIST?: KVNamespace;
  GITHUB_TOKEN?: string;
};

type FetchLike = typeof fetch;
type RefreshOptions = { fetch?: FetchLike; now?: Date; logger?: Pick<Console, "error"> };
type GitHubRepository = Record<string, unknown>;

type RepositoryListing = {
  id: number;
  fullName: string;
  owner: string;
  name: string;
  description: string | null;
  url: string;
  stars: number;
  language: string | null;
  defaultBranch: string;
  pushedAt: string | null;
};

export type PluginListing = {
  id: string;
  name: string;
  description: string | null;
  version: string;
  minNagiVersion: string;
  runtime: "wasi-component";
  capabilities: string[];
  source: {
    repository: string;
    url: string;
    commit: string;
    manifestPath: "nagi-plugin.toml";
  };
  artifact: { sha256: string; bytes: number };
  manifestSha256: string;
  scans: ScanResult;
  reviewStatus: "official" | "verified-metadata";
  capabilityDiff: { added: string[]; removed: string[] };
  popularity: { stars: number };
  updatedAt: string | null;
};

export type PluginSnapshot = {
  schemaVersion: 2;
  generatedAt: string;
  source: {
    provider: "github";
    query: string;
    totalCount: number;
    collectedCount: number;
    verifiedCount: number;
    truncated: boolean;
    warnings?: string[];
  };
  plugins: PluginListing[];
};

export type RefreshResult = { ok: true; snapshot: PluginSnapshot } | { ok: false; error: string };

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method !== "GET" || url.pathname !== "/v1/plugins") {
      return jsonResponse({ error: "Not found" }, 404, "no-store");
    }
    const object = await env.PLUGIN_MARKETPLACE_BUCKET.get(SNAPSHOT_KEY);
    if (!object) return jsonResponse({ error: "Registry snapshot unavailable" }, 503, "no-store");
    return new Response(await object.text(), {
      headers: {
        "Content-Type": "application/json; charset=utf-8",
        "Cache-Control": SNAPSHOT_CACHE_CONTROL,
        "Access-Control-Allow-Origin": "*",
      },
    });
  },

  scheduled(_event: ScheduledController, env: Env, ctx: ExecutionContext): void {
    ctx.waitUntil(refreshPlugins(env));
  },
};

export async function refreshPlugins(env: Env, options: RefreshOptions = {}): Promise<RefreshResult> {
  const logger = options.logger ?? console;
  try {
    const token = env.GITHUB_TOKEN?.trim();
    if (!token) throw new Error("GITHUB_TOKEN is not configured");
    const fetchFn = options.fetch ?? fetch;
    const discovered = await fetchGitHubRepositories(fetchFn, token);
    const submitted = await readSubmissionCandidates(env.PLUGIN_MARKETPLACE_BLACKLIST);
    const candidates = mergeCandidates(normalizeRepositories(discovered.repositories), submitted);
    if (candidates.length === 0) throw new Error("GitHub returned no listable plugin repositories");

    const blocked = await readBlacklistedRepositories(env);
    const previous = await readPreviousSnapshot(env.PLUGIN_MARKETPLACE_BUCKET);
    const warnings: string[] = [];
    const plugins: PluginListing[] = [];
    for (const repository of candidates) {
      if (blocked.has(repository.fullName.toLowerCase())) continue;
      try {
        const verified = await verifyRepository(fetchFn, token, repository, previous);
        if (verified.scans.verdict === "pass") plugins.push(verified);
        else warnings.push(`${repository.fullName}: automated scans failed`);
      } catch (error) {
        warnings.push(`${repository.fullName}: ${error instanceof Error ? error.message : "verification failed"}`);
      }
    }
    plugins.sort(compareVerifiedPlugins);

    const snapshot: PluginSnapshot = {
      schemaVersion: 2,
      generatedAt: (options.now ?? new Date()).toISOString(),
      source: {
        provider: "github",
        query: GITHUB_QUERY,
        totalCount: discovered.totalCount,
        collectedCount: candidates.length,
        verifiedCount: plugins.length,
        truncated: discovered.truncated,
        ...(warnings.length > 0 || discovered.truncated
          ? {
              warnings: [
                ...(discovered.truncated
                  ? [`GitHub returned ${discovered.totalCount} results; only ${discovered.repositories.length} were collected.`]
                  : []),
                ...warnings.slice(0, 100),
              ],
            }
          : {}),
      },
      plugins,
    };
    await env.PLUGIN_MARKETPLACE_BUCKET.put(SNAPSHOT_KEY, JSON.stringify(snapshot), {
      httpMetadata: {
        contentType: "application/json; charset=utf-8",
        cacheControl: SNAPSHOT_CACHE_CONTROL,
      },
    });
    return { ok: true, snapshot };
  } catch (error) {
    const message = error instanceof Error ? error.message : "unknown refresh error";
    logger.error(`plugin marketplace refresh failed: ${message}`);
    return { ok: false, error: message };
  }
}

async function verifyRepository(
  fetchFn: FetchLike,
  token: string,
  repository: RepositoryListing,
  previous: PluginSnapshot | null,
): Promise<PluginListing> {
  const headers = githubHeaders(token);
  const repositoryPath = `${encodeURIComponent(repository.owner)}/${encodeURIComponent(repository.name)}`;
  const commitResponse = await fetchWithTimeout(
    fetchFn,
    new URL(`https://api.github.com/repos/${repositoryPath}/commits/${encodeURIComponent(repository.defaultBranch)}`),
    { headers },
  );
  if (!commitResponse.ok) throw new Error(`commit lookup failed with status ${commitResponse.status}`);
  const commitBody: unknown = await commitResponse.json();
  const commit = isObject(commitBody) ? readString(commitBody.sha) : null;
  if (!commit || !/^[0-9a-f]{40}$/.test(commit)) throw new Error("commit lookup returned no immutable SHA");

  const rawBase = `https://raw.githubusercontent.com/${repositoryPath}/${commit}`;
  const manifestBytes = await fetchBytes(fetchFn, new URL(`${rawBase}/nagi-plugin.toml`), MAX_MANIFEST_BYTES);
  const manifestText = new TextDecoder("utf-8", { fatal: true }).decode(manifestBytes);
  const manifest = parsePluginManifest(manifestText);
  const componentUrl = `${rawBase}/${manifest.entrypoint.split("/").map(encodeURIComponent).join("/")}`;
  const component = await fetchBytes(fetchFn, new URL(componentUrl), MAX_COMPONENT_BYTES);
  const scans = await scanPluginArtifact({ manifestText, component });
  const priorCapabilities = previous?.plugins.find((plugin) => plugin.id === manifest.id)?.capabilities ?? [];
  return listingFromVerified(repository, commit, manifest, manifestBytes, component, scans, priorCapabilities);
}

async function fetchBytes(fetchFn: FetchLike, url: URL, maxBytes: number): Promise<Uint8Array> {
  const response = await fetchWithTimeout(fetchFn, url, { redirect: "error" });
  if (!response.ok) throw new Error(`artifact fetch failed with status ${response.status}`);
  const length = Number(response.headers.get("Content-Length"));
  if (Number.isFinite(length) && length > maxBytes) throw new Error(`artifact exceeds ${maxBytes} bytes`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.length === 0 || bytes.length > maxBytes) throw new Error(`artifact must be 1 to ${maxBytes} bytes`);
  return bytes;
}

async function listingFromVerified(
  repository: RepositoryListing,
  commit: string,
  manifest: MarketplaceManifest,
  manifestBytes: Uint8Array,
  component: Uint8Array,
  scans: ScanResult,
  priorCapabilities: string[],
): Promise<PluginListing> {
  return {
    id: manifest.id,
    name: manifest.name,
    description: repository.description,
    version: manifest.version,
    minNagiVersion: manifest.minNagiVersion,
    runtime: manifest.runtime,
    capabilities: manifest.capabilities,
    source: {
      repository: repository.fullName,
      url: repository.url,
      commit,
      manifestPath: "nagi-plugin.toml",
    },
    artifact: { sha256: await sha256(component), bytes: component.length },
    manifestSha256: await sha256(manifestBytes),
    scans,
    reviewStatus:
      repository.owner.toLowerCase() === "cod-hash-studios" ? "official" : "verified-metadata",
    capabilityDiff: capabilityDiff(manifest.capabilities, priorCapabilities),
    popularity: { stars: repository.stars },
    updatedAt: repository.pushedAt,
  };
}

async function fetchGitHubRepositories(
  fetchFn: FetchLike,
  token: string,
): Promise<{ repositories: GitHubRepository[]; totalCount: number; truncated: boolean }> {
  const repositories: GitHubRepository[] = [];
  let totalCount = 0;
  for (let page = 1; repositories.length < MAX_REPOS; page += 1) {
    const url = new URL(GITHUB_SEARCH_URL);
    for (const [key, value] of Object.entries({ q: GITHUB_QUERY, per_page: String(PER_PAGE), page: String(page), sort: "updated", order: "desc" })) {
      url.searchParams.set(key, value);
    }
    const response = await fetchWithTimeout(fetchFn, url, { headers: githubHeaders(token) });
    if (!response.ok) throw new Error(`GitHub search failed with status ${response.status}`);
    const body: unknown = await response.json();
    if (!isObject(body) || typeof body.total_count !== "number" || !Array.isArray(body.items)) {
      throw new Error("GitHub search returned malformed JSON");
    }
    if (body.incomplete_results === true) throw new Error("GitHub search returned incomplete results");
    totalCount = body.total_count;
    repositories.push(...body.items.slice(0, MAX_REPOS - repositories.length));
    if (repositories.length >= totalCount || body.items.length === 0) break;
  }
  return { repositories, totalCount, truncated: totalCount > repositories.length };
}

function githubHeaders(token: string): Record<string, string> {
  return {
    Accept: "application/vnd.github+json",
    Authorization: `Bearer ${token}`,
    "User-Agent": "nagi-plugin-marketplace",
    "X-GitHub-Api-Version": GITHUB_API_VERSION,
  };
}

async function fetchWithTimeout(fetchFn: FetchLike, url: URL, init: RequestInit): Promise<Response> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    return await fetchFn(url, { ...init, signal: controller.signal });
  } finally {
    clearTimeout(timeout);
  }
}

export function normalizeRepositories(repositories: GitHubRepository[]): RepositoryListing[] {
  return repositories.map(normalizeRepository).filter((plugin): plugin is RepositoryListing => plugin !== null);
}

function normalizeRepository(repo: GitHubRepository): RepositoryListing | null {
  if (readBoolean(repo.disabled) || readBoolean(repo.archived) || readBoolean(repo.fork) || readBoolean(repo.private)) return null;
  const parts = splitFullName(readString(repo.full_name));
  const ownerObject = isObject(repo.owner) ? repo.owner : {};
  const owner = readString(ownerObject.login) ?? parts.owner;
  const name = readString(repo.name) ?? parts.name;
  const url = readString(repo.html_url);
  const defaultBranch = readString(repo.default_branch) ?? "main";
  if (!owner || !name || !url || !isValidGitHubRepoUrl(url, owner, name)) return null;
  return {
    id: readInteger(repo.id) ?? 0,
    fullName: `${owner}/${name}`,
    owner,
    name,
    description: readNullableString(repo.description),
    url,
    stars: readNonNegativeInteger(repo.stargazers_count),
    language: readNullableString(repo.language),
    defaultBranch,
    pushedAt: readIsoString(repo.pushed_at),
  };
}

function mergeCandidates(discovered: RepositoryListing[], submitted: string[]): RepositoryListing[] {
  const byName = new Map(discovered.map((repository) => [repository.fullName.toLowerCase(), repository]));
  for (const fullName of submitted) {
    if (byName.has(fullName.toLowerCase())) continue;
    const [owner, name] = fullName.split("/");
    byName.set(fullName.toLowerCase(), {
      id: 0,
      fullName,
      owner,
      name,
      description: null,
      url: `https://github.com/${owner}/${name}`,
      stars: 0,
      language: null,
      defaultBranch: "main",
      pushedAt: null,
    });
  }
  return [...byName.values()];
}

async function readBlacklistedRepositories(env: Env): Promise<Set<string>> {
  const blocked = new Set<string>();
  const namespace = env.PLUGIN_MARKETPLACE_BLACKLIST;
  if (!namespace) return blocked;
  let cursor: string | undefined;
  do {
    const page = await namespace.list({ prefix: BLACKLIST_REPO_KEY_PREFIX, cursor });
    for (const key of page.keys) {
      const repository = key.name.slice(BLACKLIST_REPO_KEY_PREFIX.length).trim().toLowerCase();
      if (repository.includes("/")) blocked.add(repository);
    }
    cursor = page.cursor;
  } while (cursor);
  return blocked;
}

async function readPreviousSnapshot(bucket: R2Bucket): Promise<PluginSnapshot | null> {
  try {
    const object = await bucket.get(SNAPSHOT_KEY);
    if (!object) return null;
    const value: unknown = JSON.parse(await object.text());
    return isObject(value) && value.schemaVersion === 2 ? (value as PluginSnapshot) : null;
  } catch {
    return null;
  }
}

function compareVerifiedPlugins(left: PluginListing, right: PluginListing): number {
  return (
    Number(right.reviewStatus === "official") - Number(left.reviewStatus === "official") ||
    left.name.localeCompare(right.name) ||
    left.id.localeCompare(right.id)
  );
}

function jsonResponse(body: unknown, status: number, cacheControl: string): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": cacheControl },
  });
}

function isValidGitHubRepoUrl(url: string, owner: string, name: string): boolean {
  try {
    const parsed = new URL(url);
    const segments = parsed.pathname.split("/").filter(Boolean);
    return parsed.protocol === "https:" && parsed.hostname === "github.com" && segments.length === 2 && segments[0].toLowerCase() === owner.toLowerCase() && segments[1].toLowerCase() === name.toLowerCase();
  } catch {
    return false;
  }
}

function splitFullName(fullName: string | null): { owner: string | null; name: string | null } {
  if (!fullName) return { owner: null, name: null };
  const [owner, name, extra] = fullName.split("/");
  return owner && name && !extra ? { owner, name } : { owner: null, name: null };
}

function readString(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}
function readNullableString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}
function readBoolean(value: unknown): boolean {
  return value === true;
}
function readInteger(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) ? value : null;
}
function readNonNegativeInteger(value: unknown): number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : 0;
}
function readIsoString(value: unknown): string | null {
  if (typeof value !== "string" || Number.isNaN(Date.parse(value))) return null;
  return value;
}
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
