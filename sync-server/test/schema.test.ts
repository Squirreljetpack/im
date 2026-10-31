import { describe, expect, it } from "vitest";

import { SCHEMA } from "../src/schema";

/**
 * Every vocabulary this worker has ever spoken, keyed by its `version`.
 *
 * The two sides compare `version` and refuse a round when it differs, which is
 * only safe while that number moves whenever `kinds` does. Pinning each version
 * to the digest of its vocabulary makes both directions fail CI: edit `kinds`
 * without bumping `version` and the digest below no longer matches, bump it
 * without recording the new digest and there is no entry to match.
 */
const VOCABULARY: Record<number, string> = {
  1: "f7823cffa53a6674",
};

/** JSON with every object's keys sorted, so formatting and key order do not matter. */
function canonical(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonical).join(",")}]`;
  }
  if (value !== null && typeof value === "object") {
    const entries = Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical((value as Record<string, unknown>)[key])}`);
    return `{${entries.join(",")}}`;
  }
  return JSON.stringify(value);
}

async function digest(text: string): Promise<string> {
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(bytes), (byte) => byte.toString(16).padStart(2, "0"))
    .join("")
    .slice(0, 16);
}

describe("schema", () => {
  it("pins each vocabulary version to its digest", async () => {
    expect(VOCABULARY[SCHEMA.version]).toBe(await digest(canonical(SCHEMA.kinds)));
  });
});
