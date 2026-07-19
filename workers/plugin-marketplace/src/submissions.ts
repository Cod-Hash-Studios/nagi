const PREFIX = "submission:";
const REPOSITORY = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,38}\/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$/;

export type SubmissionNamespace = {
  list(options?: { prefix?: string; cursor?: string }): Promise<{
    keys: Array<{ name: string }>;
    cursor?: string;
  }>;
};

export async function readSubmissionCandidates(namespace?: SubmissionNamespace): Promise<string[]> {
  if (!namespace) return [];
  const repositories = new Set<string>();
  let cursor: string | undefined;
  do {
    const page = await namespace.list({ prefix: PREFIX, cursor });
    for (const key of page.keys) {
      const repository = key.name.slice(PREFIX.length).trim();
      if (REPOSITORY.test(repository)) repositories.add(repository);
    }
    cursor = page.cursor;
  } while (cursor);
  return [...repositories].sort((left, right) => left.localeCompare(right));
}
