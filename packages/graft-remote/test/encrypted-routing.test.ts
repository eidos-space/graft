import { describe, expect, it } from "vitest";
import { EncryptedRoutingIndex, type EncryptedRoutingHint } from "../src/encrypted-routing.js";

const token = (value: number) => value.toString(16).padStart(64, "0");
function hint(id: number, commits: EncryptedRoutingHint["commits"]): EncryptedRoutingHint {
  return {
    key: `e2ee/hints/${token(id)}`,
    pack: `e2ee/packs/${token(id)}`,
    index: `e2ee/indexes/${token(id)}`,
    commits,
  };
}

describe("encrypted routing resource bounds", () => {
  it("counts repeated commit placements and rejects before mutating the index", () => {
    const routing = new EncryptedRoutingIndex();
    const commits = Array.from({ length: 4096 }, (_, index) => ({ token: token(index), parents: [] }));
    for (let index = 0; index < 8; index += 1) routing.add(hint(index, commits));
    // Replaying an identical immutable hint consumes no additional capacity.
    for (let index = 0; index < 8; index += 1) routing.add(hint(index, commits));
    expect(() => routing.add(hint(8, commits))).toThrow("Encrypted graph planning is unavailable");
    expect(routing.hints.size).toBe(8);
    expect(routing.select(token(0), null).map(({ key }) => key)).toEqual([hint(0, []).key]);
  });

  it("bounds retained parent strings even when every commit repeats", () => {
    const routing = new EncryptedRoutingIndex();
    let rejected = false;
    for (let index = 0; index < 32; index += 1) {
      // Each public hint is below its 1 MiB wire limit. These are separately
      // decoded strings/arrays, as with independently uploaded repack hints.
      const commits = Array.from({ length: 512 }, (_, commit) => ({
        token: token(commit),
        parents: Array.from({ length: 32 }, (_, parent) => token(1000 + parent)),
      }));
      try { routing.add(hint(index, commits)); }
      catch (error) {
        expect(error).toMatchObject({ status: 422, code: "encrypted_hint_unavailable" });
        expect(routing.hints.size).toBe(index);
        rejected = true;
        break;
      }
    }
    expect(rejected).toBe(true);
  });

  it("bounds pending DFS entries for a deep merge graph", () => {
    const routing = new EncryptedRoutingIndex();
    const leaves = Array.from({ length: 31 }, (_, index) => token(4000 + index));
    const commits = Array.from({ length: 1100 }, (_, index) => ({
      token: token(index),
      parents: index === 1099 ? [] : [token(index + 1), ...leaves],
    })).concat(leaves.map((token) => ({ token, parents: [] })));
    for (let index = 0; index < commits.length; index += 512) routing.add(hint(index, commits.slice(index, index + 512)));
    expect(() => routing.select(token(0), null)).toThrow("Encrypted graph planning is unavailable");
    // A shallower traversal of the same graph remains available.
    expect(routing.select(token(1000), null).length).toBeGreaterThan(0);
  });
});
