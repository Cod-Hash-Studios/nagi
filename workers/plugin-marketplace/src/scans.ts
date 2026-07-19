const MAX_COMPONENT_BYTES = 16 * 1024 * 1024;
const SECRET_PATTERNS = [
  /gh[pousr]_[A-Za-z0-9]{30,}/,
  /github_pat_[A-Za-z0-9_]{40,}/,
  /AKIA[0-9A-Z]{16}/,
  /-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/,
];

export type ScanCheck = {
  id: "component-format" | "artifact-size" | "secret-patterns";
  status: "pass" | "fail";
  detail: string;
};

export type ScanResult = {
  verdict: "pass" | "fail";
  checks: ScanCheck[];
};

export async function scanPluginArtifact(input: {
  manifestText: string;
  component: Uint8Array;
}): Promise<ScanResult> {
  const hasWasmMagic =
    input.component.length >= 8 &&
    input.component[0] === 0 &&
    input.component[1] === 0x61 &&
    input.component[2] === 0x73 &&
    input.component[3] === 0x6d;
  const withinSize = input.component.length > 0 && input.component.length <= MAX_COMPONENT_BYTES;
  const containsSecret = SECRET_PATTERNS.some((pattern) => pattern.test(input.manifestText));
  const checks: ScanCheck[] = [
    {
      id: "component-format",
      status: hasWasmMagic ? "pass" : "fail",
      detail: hasWasmMagic ? "WebAssembly magic bytes present" : "artifact is not WebAssembly",
    },
    {
      id: "artifact-size",
      status: withinSize ? "pass" : "fail",
      detail: withinSize ? `${input.component.length} bytes` : `artifact must be 1 to ${MAX_COMPONENT_BYTES} bytes`,
    },
    {
      id: "secret-patterns",
      status: containsSecret ? "fail" : "pass",
      detail: containsSecret ? "secret-like token found in manifest" : "no known secret pattern found",
    },
  ];
  return {
    verdict: checks.some((check) => check.status === "fail") ? "fail" : "pass",
    checks,
  };
}

export function capabilityDiff(current: string[], previous: string[]): {
  added: string[];
  removed: string[];
} {
  const currentSet = new Set(current);
  const previousSet = new Set(previous);
  return {
    added: current.filter((capability) => !previousSet.has(capability)).sort(),
    removed: previous.filter((capability) => !currentSet.has(capability)).sort(),
  };
}

export async function sha256(bytes: Uint8Array): Promise<string> {
  const owned = Uint8Array.from(bytes);
  const digest = await crypto.subtle.digest("SHA-256", owned.buffer);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
