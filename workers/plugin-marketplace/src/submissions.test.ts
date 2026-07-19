import { describe, expect, test } from "bun:test";
import { readSubmissionCandidates } from "./submissions";

describe("readSubmissionCandidates", () => {
  test("accepts only canonical GitHub repository keys and paginates", async () => {
    const pages = [
      { keys: [{ name: "submission:owner/repo" }, { name: "submission:../../escape" }], cursor: "2" },
      { keys: [{ name: "submission:Other/Plugin" }] },
    ];
    let index = 0;
    const candidates = await readSubmissionCandidates({
      async list() {
        return pages[index++];
      },
    });

    expect(candidates).toEqual(["Other/Plugin", "owner/repo"]);
  });
});
