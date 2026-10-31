import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { PROTOCOL, SCHEMA_VERSION } from "../src/schema";
import type {
  AuthResponse,
  CellEvent,
  CellValue,
  LogRow,
  SyncRequest,
  SyncResponse,
  Verdict,
} from "../src/types";

const BASE = "https://sync.test";

async function call(
  path: string,
  options: { method?: string; body?: unknown; token?: string } = {},
): Promise<Response> {
  const headers: Record<string, string> = {};
  if (options.body !== undefined) {
    headers["content-type"] = "application/json";
  }
  if (options.token !== undefined) {
    headers.authorization = `Bearer ${options.token}`;
  }
  return exports.default.fetch(
    new Request(`${BASE}${path}`, {
      method: options.method ?? "GET",
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
    }),
  );
}

async function register(email: string): Promise<AuthResponse> {
  const response = await call("/api/v1/auth/register", {
    method: "POST",
    body: { email, password: "correct horse battery" },
  });
  expect(response.status).toBe(201);
  return (await response.json()) as AuthResponse;
}

/** A stamp safely in the past, so pushes never race the server's clock. */
function ago(ms: number): number {
  return Date.now() - ms;
}

let sequence = 0;

function cell(
  entityId: string,
  kind: string,
  field: string,
  value: CellValue | null,
  timestamp: number,
  device = "device-a",
  eventId = `evt-${++sequence}`,
): CellEvent {
  return {
    event_id: eventId,
    device_id: device,
    entity_id: entityId,
    kind,
    field,
    value,
    timestamp,
  };
}

/**
 * Cells a create must carry whatever the test is about: a task rows' start
 * time is required by the schema, and no test inspects its value. A caller
 * that supplies the field itself keeps its own.
 */
const REQUIRED: Record<string, [string, CellValue][]> = {
  task: [["start_time", { int: 1_700_000_000 }]],
};

function create(
  entityId: string,
  kind: string,
  fields: [string, CellValue | null][],
  timestamp: number,
  device = "device-a",
): CellEvent[] {
  const given = new Set(fields.map(([field]) => field));
  const required = (REQUIRED[kind] ?? []).filter(([field]) => !given.has(field));
  return [
    cell(entityId, kind, "", { text: "live" }, timestamp, device),
    ...[...fields, ...required].map(([field, value]) =>
      cell(entityId, kind, field, value, timestamp, device),
    ),
  ];
}

function remove(entityId: string, kind: string, timestamp: number, device = "device-a") {
  return cell(entityId, kind, "", { text: "deleted" }, timestamp, device);
}

async function sync(
  token: string,
  device: string,
  version: number,
  events: CellEvent[] = [],
  page?: number,
): Promise<SyncResponse & { verdictFor: (index: number) => Verdict }> {
  const query = page === undefined ? "" : `?page=${page}`;
  const response = await call(`/api/sync${query}`, {
    method: "POST",
    token,
    body: request(token, device, version, events),
  });
  expect(response.status).toBe(200);
  const body = (await response.json()) as SyncResponse;
  expect(body.verdicts).toHaveLength(events.length);
  logs.set(token, body.cursor.log!);
  return {
    ...body,
    verdictFor: (index: number) => {
      const verdict = body.verdicts[index];
      if (verdict === undefined) {
        throw new Error(`no verdict at ${index}`);
      }
      return verdict;
    },
  };
}

/**
 * The log each token was last told it is reading. A version alone is not a
 * position, so the helper always sends the name it learned from the response —
 * starting with none, which is where a fresh database (or a reset one) stands.
 */
const logs = new Map<string, string>();

function request(
  token: string,
  device: string,
  version: number,
  events: CellEvent[],
): SyncRequest {
  const log = logs.get(token);
  return {
    protocol: PROTOCOL,
    schema: SCHEMA_VERSION,
    device_id: device,
    cursor: log === undefined ? null : { log, version },
    events,
  };
}

function fields(rows: LogRow[], entityId: string): string[] {
  return rows.filter((row) => row.entity_id === entityId).map((row) => row.field);
}

function valueOf(rows: LogRow[], entityId: string, field: string): CellValue | null {
  const row = rows.find((candidate) => candidate.entity_id === entityId && candidate.field === field);
  if (row === undefined) {
    throw new Error(`no cell ${entityId}/${field}`);
  }
  return row.value;
}

describe("accounts", () => {
  it("registers, logs in and reports status", async () => {
    const account = await register("alice@example.com");
    expect(account.user_id).toMatch(/^[0-9a-f-]{36}$/);

    const login = await call("/api/v1/auth/login", {
      method: "POST",
      body: { email: "alice@example.com", password: "correct horse battery" },
    });
    expect(login.status).toBe(200);
    expect(((await login.json()) as AuthResponse).user_id).toBe(account.user_id);

    const status = await call("/api/v1/auth/status", { token: account.token });
    expect(status.status).toBe(200);
    expect(await status.json()).toEqual({
      user_id: account.user_id,
      email: "alice@example.com",
    });
  });

  it("requires a token for sync", async () => {
    expect((await call("/api/sync", { method: "POST", body: {} })).status).toBe(401);
  });

  it("keeps users isolated", async () => {
    const alice = await register("frank@example.com");
    const bob = await register("grace@example.com");
    const task = "0193f000-0000-7000-8000-000000000004";
    await sync(alice.token, "device-a", 0, create(task, "task", [["name", { text: "mine" }]], ago(1000)));

    const pulled = await sync(bob.token, "device-b", 0);
    expect(pulled.events).toEqual([]);
    expect(pulled.cursor.version).toBe(0);
    expect(pulled.cursor.log).not.toBeNull();
  });
});

describe("pull", () => {
  it("serves cells by winner version, existence first", async () => {
    const account = await register("carol@example.com");
    const task = "0193f000-0000-7000-8000-000000000001";

    const pushed = await sync(
      account.token,
      "device-a",
      0,
      create(task, "task", [["name", { text: "write it" }]], ago(1000)),
    );
    expect(pushed.verdicts.every((verdict) => verdict.outcome === "accepted")).toBe(true);

    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.events.map((row) => row.field)).toEqual(["", "name", "start_time"]);
    expect(pulled.events.map((row) => row.version)).toEqual([1, 2, 3]);
    expect(pulled.cursor.version).toBe(3);
    expect(pulled.has_more).toBe(false);

    // Nothing new: the cursor is already at the register's head.
    const again = await sync(account.token, "device-b", pulled.cursor.version);
    expect(again.events).toEqual([]);
    expect(again.cursor.version).toBe(pulled.cursor.version);
  });

  it("suppresses own echoes, except when materialising from nothing", async () => {
    const account = await register("dave@example.com");
    const task = "0193f000-0000-7000-8000-000000000002";
    const events = create(task, "task", [["name", { text: "mine" }]], ago(1000));
    await sync(account.token, "device-a", 0, events);

    // Cursor 0 means "I have no rows", so my own cells must come back.
    const rebuild = await sync(account.token, "device-a", 0);
    expect(rebuild.events.map((row) => row.field)).toEqual(["", "name", "start_time"]);

    // Past that, they are not echoed: the row already reflects them.
    const delta = await sync(account.token, "device-a", rebuild.cursor.version);
    expect(delta.events).toEqual([]);
    expect(delta.cursor.version).toBe(rebuild.cursor.version);
  });

  it("serves a deleted entity as its existence cell alone", async () => {
    const account = await register("erin@example.com");
    const task = "0193f000-0000-7000-8000-000000000003";
    await sync(
      account.token,
      "device-a",
      0,
      create(task, "task", [["name", { text: "gone" }], ["body", { text: "text" }]], ago(2000)),
    );
    await sync(account.token, "device-a", 0, [remove(task, "task", ago(1000))]);

    const pulled = await sync(account.token, "device-c", 0);
    expect(fields(pulled.events, task)).toEqual([""]);
    expect(valueOf(pulled.events, task, "")).toEqual({ text: "deleted" });
  });
});

describe("push validation", () => {
  it("settles a cell by stamp, not by arrival order", async () => {
    const account = await register("heidi@example.com");
    const task = "0193f000-0000-7000-8000-000000000010";
    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "a" }]], ago(5000)));

    // A newer stamp that arrives later wins.
    const newer = await sync(account.token, "device-b", 0, [
      cell(task, "task", "name", { text: "newer" }, ago(4000), "device-b"),
    ]);
    expect(newer.verdicts[0]?.outcome).toBe("accepted");

    // An older stamp arriving after it loses, and the verdict names the winner.
    const older = await sync(account.token, "device-c", 0, [
      cell(task, "task", "name", { text: "older" }, ago(4500), "device-c"),
    ]);
    expect(older.verdicts[0]).toMatchObject({ outcome: "superseded" });
    expect(older.verdicts[0]?.winner?.value).toEqual({ text: "newer" });

    const pulled = await sync(account.token, "device-d", 0);
    expect(valueOf(pulled.events, task, "name")).toEqual({ text: "newer" });
  });

  it("lets devices own different fields of one row", async () => {
    const account = await register("ivan@example.com");
    const task = "0193f000-0000-7000-8000-000000000011";
    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "n" }]], ago(5000)));
    await sync(account.token, "device-a", 0, [
      cell(task, "task", "body", { text: "from a" }, ago(4000)),
    ]);
    await sync(account.token, "device-b", 0, [
      cell(task, "task", "body", { text: "from b" }, ago(3000), "device-b"),
    ]);

    const pulled = await sync(account.token, "device-c", 0);
    expect(valueOf(pulled.events, task, "name")).toEqual({ text: "n" });
    expect(valueOf(pulled.events, task, "body")).toEqual({ text: "from b" });
  });

  it("refuses a value write for an entity it has never seen", async () => {
    const account = await register("judy@example.com");
    const task = "0193f000-0000-7000-8000-000000000012";

    const early = await sync(account.token, "device-a", 0, [
      cell(task, "task", "name", { text: "too soon" }, ago(1000)),
    ]);
    expect(early.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "dangling" });

    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "now" }]], ago(500)));
    const late = await sync(account.token, "device-a", 0, [
      cell(task, "task", "body", { text: "fine" }, ago(100)),
    ]);
    expect(late.verdicts[0]?.outcome).toBe("accepted");
  });

  it("treats a delete as terminal and a repeat delete as a no-op", async () => {
    const account = await register("karl@example.com");
    const task = "0193f000-0000-7000-8000-000000000013";
    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "doomed" }]], ago(5000)));
    await sync(account.token, "device-a", 0, [remove(task, "task", ago(4000))]);

    const edit = await sync(account.token, "device-b", 0, [
      cell(task, "task", "name", { text: "revive?" }, ago(3000), "device-b"),
    ]);
    expect(edit.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "deleted" });
    expect(edit.verdicts[0]?.tombstone).toMatchObject({ field: "", device_id: "device-a" });
    expect(edit.verdicts[0]?.tombstone?.value).toEqual({ text: "deleted" });

    // Two devices both deleting the same row is not an error.
    const repeat = await sync(account.token, "device-b", 0, [
      remove(task, "task", ago(2000), "device-b"),
    ]);
    expect(["accepted", "superseded"]).toContain(repeat.verdicts[0]?.outcome);

    const revived = await sync(
      account.token,
      "device-b",
      0,
      create(task, "task", [["name", { text: "x" }]], ago(1000), "device-b"),
    );
    expect(revived.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "deleted" });
  });

  it("refuses stamps beyond the allowed skew, and reports its clock", async () => {
    const account = await register("mallory@example.com");
    const task = "0193f000-0000-7000-8000-000000000014";
    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "n" }]], ago(1000)));

    const future = await sync(account.token, "device-a", 0, [
      cell(task, "task", "name", { text: "from the future" }, Date.now() + 3_600_000),
    ]);
    expect(future.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "clock_ahead" });
    expect(future.now).toBeGreaterThan(Date.now() - 60_000);

    // A tracker's `time` decides which slot it occupies, so it is held to the
    // same rule.
    const tracker = "0193f000-0000-7000-8000-000000000015";
    const created = await sync(
      account.token,
      "device-a",
      0,
      create(
        tracker,
        "tracker",
        [
          ["type", { text: "weight" }],
          ["score", { int: 70 }],
          ["time", { int: Math.floor(Date.now() / 1000) - 60 }],
        ],
        ago(500),
      ),
    );
    expect(created.verdicts.every((verdict) => verdict.outcome === "accepted")).toBe(true);
    const ahead = await sync(
      account.token,
      "device-a",
      0,
      create(
        "0193f000-0000-7000-8000-000000000015b",
        "tracker",
        [
          ["type", { text: "weight" }],
          ["score", { int: 70 }],
          ["time", { int: Math.floor(Date.now() / 1000) + 3600 }],
        ],
        ago(400),
      ),
    );
    const verdicts = ahead.verdicts.filter((verdict) => verdict.outcome === "rejected");
    expect(verdicts).toHaveLength(1);
    expect(verdicts[0]).toMatchObject({ reason: "clock_ahead" });
  });

  it("keeps the kind a task was created with", async () => {
    const account = await register("niaj@example.com");
    const task = "0193f000-0000-7000-8000-000000000016";
    await sync(account.token, "device-a", 0, create(task, "task", [["name", { text: "once" }]], ago(5000)));

    const converted = await sync(account.token, "device-a", 0, [
      cell(task, "task", "interval_secs", { int: 86_400 }, ago(4000)),
    ]);
    expect(converted.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "kind_change" });

    const edited = await sync(account.token, "device-a", 0, [
      cell(task, "task", "name", { text: "renamed" }, ago(3000)),
    ]);
    expect(edited.verdicts[0]?.outcome).toBe("accepted");

    // A recurring task may retune its interval: still recurring.
    const recurring = "0193f000-0000-7000-8000-000000000017";
    await sync(
      account.token,
      "device-a",
      0,
      create(recurring, "task", [["name", { text: "repeat" }], ["interval_secs", { int: 86_400 }]], ago(2000)),
    );
    const retuned = await sync(account.token, "device-a", 0, [
      cell(recurring, "task", "interval_secs", { int: 43_200 }, ago(1000)),
    ]);
    expect(retuned.verdicts[0]?.outcome).toBe("accepted");
  });

  it("refuses to rewrite a create-only field", async () => {
    const account = await register("olga@example.com");
    const tracker = "0193f000-0000-7000-8000-000000000018";
    const created = await sync(
      account.token,
      "device-a",
      0,
      create(
        tracker,
        "tracker",
        [
          ["type", { text: "weight" }],
          ["score", { real: 70.5 }],
          ["time", { int: Math.floor(Date.now() / 1000) - 60 }],
        ],
        ago(2000),
      ),
    );
    expect(created.verdicts.every((verdict) => verdict.outcome === "accepted")).toBe(true);

    const moved = await sync(account.token, "device-a", 0, [
      cell(tracker, "tracker", "time", { int: Math.floor(Date.now() / 1000) - 30 }, ago(1000)),
    ]);
    expect(moved.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "immutable" });

    const scored = await sync(account.token, "device-a", 0, [
      cell(tracker, "tracker", "score", { real: 71.5 }, ago(500)),
    ]);
    expect(scored.verdicts[0]?.outcome).toBe("accepted");
  });

  it("keeps a value's storage class on the wire", async () => {
    const account = await register("peggy@example.com");
    const tracker = "0193f000-0000-7000-8000-000000000019";
    await sync(
      account.token,
      "device-a",
      0,
      create(
        tracker,
        "tracker",
        [
          ["type", { text: "weight" }],
          ["score", { real: 70.0 }],
          ["time", { int: Math.floor(Date.now() / 1000) - 60 }],
        ],
        ago(1000),
      ),
    );

    const pulled = await sync(account.token, "device-b", 0);
    expect(valueOf(pulled.events, tracker, "score")).toEqual({ real: 70 });
  });

  it("rejects shapes it cannot fold", async () => {
    const account = await register("quinn@example.com");
    const malformed = [
      { event_id: "bad-1", device_id: "device-a", entity_id: "e", kind: "task", field: "name", value: { text: 1 }, timestamp: ago(100) },
      { event_id: "bad-2", device_id: "device-a", entity_id: "e", kind: "nope", field: "name", value: { text: "x" }, timestamp: ago(100) },
      { event_id: "bad-3", device_id: "someone-else", entity_id: "e", kind: "task", field: "name", value: { text: "x" }, timestamp: ago(100) },
    ] as unknown as CellEvent[];
    const response = await sync(account.token, "device-a", 0, malformed);
    expect(response.verdicts.map((verdict) => verdict.reason)).toEqual([
      "class",
      "kind",
      "device",
    ]);
  });

  it("refuses a create that is missing a required field, and lands none of it", async () => {
    const account = await register("frost@example.com");
    const task = "0193f000-0000-7000-8000-000000000098";
    // A reader builds a row only from the page carrying the existence cell, so
    // an existence cell without `name` would be unbuildable on every device.
    const refused = await call("/api/sync", {
      method: "POST",
      token: account.token,
      body: request(account.token, "device-a", 0, [
        cell(task, "task", "", { text: "live" }, ago(1000)),
      ]),
    });
    expect(refused.status).toBe(400);
    const refusal = (await refused.json()) as { error: string };
    expect(refusal.error).toContain('missing required field "name"');

    // The transaction rolled back: not even the existence cell landed.
    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.events).toEqual([]);

    const accepted = await sync(
      account.token,
      "device-a",
      0,
      create(task, "task", [["name", { text: "whole" }]], ago(1000)),
    );
    expect(accepted.verdicts.map((verdict) => verdict.outcome)).toEqual([
      "accepted",
      "accepted",
      "accepted",
    ]);

    // `name` is not the only required cell: a task is dated from its start.
    const undated_task = "0193f000-0000-7000-8000-000000000099";
    const undated = await call("/api/sync", {
      method: "POST",
      token: account.token,
      body: request(account.token, "device-a", 0, [
        cell(undated_task, "task", "", { text: "live" }, ago(900)),
        cell(undated_task, "task", "name", { text: "undated" }, ago(900)),
      ]),
    });
    expect(undated.status).toBe(400);
    expect(((await undated.json()) as { error: string }).error).toContain(
      'missing required field "start_time"',
    );
  });

  it("is idempotent for retried pushes", async () => {
    const account = await register("rupert@example.com");
    const task = "0193f000-0000-7000-8000-000000000020";
    const events = create(task, "task", [["name", { text: "once" }]], ago(1000));
    const first = await sync(account.token, "device-a", 0, events);
    const retry = await sync(account.token, "device-a", 0, events);

    expect(retry.cursor.version).toBe(first.cursor.version);
    expect(retry.verdicts.map((verdict) => verdict.outcome)).toEqual([
      "accepted",
      "accepted",
      "accepted",
    ]);
    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.events.map((row) => row.version)).toEqual([1, 2, 3]);
  });
});

describe("deletes and links", () => {
  it("nulls the links a delete strands, at the delete's own rank", async () => {
    const account = await register("sybil@example.com");
    const parent = "0193f000-0000-7000-8000-000000000030";
    const child = "0193f000-0000-7000-8000-000000000031";
    const mood = "0193f000-0000-7000-8000-000000000032";
    const tracker = "0193f000-0000-7000-8000-000000000033";
    const done = "0193f000-0000-7000-8000-000000000034";

    await sync(
      account.token,
      "device-a",
      0,
      [
        ...create(parent, "task", [["name", { text: "parent" }]], ago(10_000)),
        ...create(child, "task", [["name", { text: "child" }], ["parent_id", { text: parent }]], ago(9_000)),
        ...create(mood, "mood", [["mood", { text: "calm" }], ["todo_id", { text: parent }]], ago(8_000)),
        ...create(
          tracker,
          "tracker",
          [
            ["type", { text: "weight" }],
            ["score", { int: 70 }],
            ["time", { int: Math.floor(Date.now() / 1000) - 60 }],
            ["mood_id", { text: mood }],
          ],
          ago(7_000),
        ),
        ...create(done, "completion", [["todo_id", { text: parent }], ["time", { int: 1 }], ["count", { int: 1 }]], ago(6_000)),
      ],
    );

    const deletedAt = ago(5_000);
    const deleted = await sync(account.token, "device-b", 0, [
      remove(parent, "task", deletedAt, "device-b"),
    ]);
    expect(deleted.verdicts[0]?.outcome).toBe("accepted");

    const pulled = await sync(account.token, "device-c", 0);
    // The child keeps its name and loses its parent link.
    expect(valueOf(pulled.events, child, "parent_id")).toBeNull();
    expect(valueOf(pulled.events, child, "name")).toEqual({ text: "child" });
    expect(valueOf(pulled.events, mood, "todo_id")).toBeNull();
    expect(valueOf(pulled.events, done, "")).toEqual({ text: "deleted" });
    // The mood still exists, so the tracker keeps its mood.
    expect(valueOf(pulled.events, tracker, "mood_id")).toEqual({ text: mood });

    // The synthesised cells are attributed to the deleting device and stamped
    // just above the link they replaced.
    const repair = pulled.events.find(
      (row) => row.entity_id === child && row.field === "parent_id",
    );
    expect(repair?.device_id).toBe("device-b");
    expect(repair?.timestamp).toBe(deletedAt + 1);

    // Deleting the mood strands the tracker's link.
    await sync(account.token, "device-b", 0, [remove(mood, "mood", ago(1_000), "device-b")]);
    const after = await sync(account.token, "device-c", pulled.cursor.version);
    expect(valueOf(after.events, tracker, "mood_id")).toBeNull();
  });

  it("refuses a link that names an entity this device cannot have seen", async () => {
    const account = await register("trent@example.com");
    const task = "0193f000-0000-7000-8000-000000000040";
    const loser = "0193f000-0000-7000-8000-000000000041";

    const dangling = await sync(account.token, "device-a", 0, [
      cell(task, "task", "parent_id", { text: "0193f000-0000-7000-8000-000000000099" }, ago(1000)),
    ]);
    expect(dangling.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "dangling" });

    await sync(
      account.token,
      "device-a",
      0,
      [...create(task, "task", [["name", { text: "self" }]], ago(900)), ...create(loser, "task", [["name", { text: "other" }]], ago(800))],
    );
    const selfParent = await sync(account.token, "device-a", 0, [
      cell(task, "task", "parent_id", { text: task }, ago(700)),
    ]);
    expect(selfParent.verdicts[0]).toMatchObject({ outcome: "rejected", reason: "self_parent" });

    const linked = await sync(account.token, "device-a", 0, [
      cell(task, "task", "parent_id", { text: loser }, ago(600)),
    ]);
    expect(linked.verdicts[0]?.outcome).toBe("accepted");
  });
});

describe("pages", () => {
  it("only breaks between pushes, never inside one", async () => {
    const account = await register("pager@example.com");
    const task = "0190a5a0-0000-7000-8000-000000000101";
    // A create is a burst of independent cells: the existence cell plus every
    // populated field. Cut between them and a client could not insert the row.
    await sync(account.token, "device-a", 0, [
      ...create(
        task,
        "task",
        [
          ["name", { text: "written" }],
          ["body", { text: "a body" }],
          ["priority", { int: 3 }],
          ["start_time", { int: 1_700_000_000 }],
        ],
        ago(5000),
      ),
    ]);

    // A page of two would split that push in three places.
    const first = await sync(account.token, "device-b", 0, [], 2);
    expect(fields(first.events, task).sort()).toEqual(
      ["", "body", "name", "priority", "start_time"].sort(),
    );
    expect(first.has_more).toBe(false);
    expect(first.cursor.version).toBe(5);
  });

  it("delivers every push exactly once across pages", async () => {
    const account = await register("pages@example.com");
    const first = "0190a5a0-0000-7000-8000-000000000201";
    const second = "0190a5a0-0000-7000-8000-000000000202";
    await sync(account.token, "device-a", 0, [
      ...create(first, "task", [["name", { text: "one" }]], ago(5000)),
    ]);
    await sync(account.token, "device-a", 0, [
      ...create(second, "mood", [["mood", { text: "two" }]], ago(4000)),
    ]);

    let cursor = 0;
    const seen: string[] = [];
    for (;;) {
      const page = await sync(account.token, "device-b", cursor, [], 2);
      seen.push(...page.events.map((row) => `${row.entity_id}/${row.field}`));
      cursor = page.cursor.version;
      if (!page.has_more) {
        break;
      }
    }
    expect(seen).toHaveLength(5);
    expect(new Set(seen).size).toBe(5);
    expect(seen).toContain(`${first}/name`);
    expect(seen).toContain(`${second}/mood`);
  });
});

describe("superseded notices", () => {
  it("tells the device whose cell lost, and keeps telling until it has read past", async () => {
    const account = await register("notices@example.com");
    const task = "0190a5a0-0000-7000-8000-000000000301";
    const mine = await sync(account.token, "device-a", 0, [
      ...create(task, "task", [["name", { text: "named" }], ["body", { text: "mine" }]], ago(9000)),
    ]);
    const myBody = mine.events.find((row) => row.field === "body");
    expect(myBody).toBeDefined();

    // Another device writes that cell later, so the earlier edit is replaced.
    await sync(account.token, "device-b", 0, [
      cell(task, "task", "body", { text: "theirs" }, ago(8000), "device-b"),
    ]);

    const firstRead = await sync(account.token, "device-a", 0, []);
    expect(firstRead.superseded).toHaveLength(1);
    expect(firstRead.superseded[0]).toMatchObject({
      event_id: myBody?.event_id,
      entity_id: task,
      field: "body",
      lost: { text: "mine" },
    });
    expect(firstRead.superseded[0]?.winner).toMatchObject({
      device_id: "device-b",
      value: { text: "theirs" },
    });
    // The winner is in the same response, so the client can apply and report
    // in one pass.
    expect(valueOf(firstRead.events, task, "body")).toEqual({ text: "theirs" });

    // A lost response must not lose the explanation: the cursor did not move.
    const reread = await sync(account.token, "device-a", 0, []);
    expect(reread.superseded).toHaveLength(1);

    // Reading past it is what retires it.
    const caughtUp = await sync(account.token, "device-a", firstRead.cursor.version, []);
    expect(caughtUp.superseded).toHaveLength(0);
    const after = await sync(account.token, "device-a", caughtUp.cursor.version, []);
    expect(after.superseded).toHaveLength(0);

    // The winning device is not told anything: it did not lose.
    const winnerRead = await sync(account.token, "device-b", 0, []);
    expect(winnerRead.superseded).toHaveLength(0);
  });

  it("does not report a link repair as a lost edit", async () => {
    const account = await register("quiet@example.com");
    const parent = "0190a5a0-0000-7000-8000-000000000401";
    const child = "0190a5a0-0000-7000-8000-000000000402";
    await sync(account.token, "device-a", 0, [
      ...create(parent, "task", [["name", { text: "parent" }]], ago(9000)),
      ...create(child, "task", [["name", { text: "child" }]], ago(8900)),
    ]);
    await sync(account.token, "device-a", 0, [
      cell(child, "task", "parent_id", { text: parent }, ago(8000)),
    ]);
    // The delete's cascade nulls the child's link: that is a repair the device
    // learns about from the tombstone, not an edit it lost to a peer.
    await sync(account.token, "device-a", 0, [remove(parent, "task", ago(7000))]);

    const read = await sync(account.token, "device-a", 0, []);
    expect(read.superseded).toHaveLength(0);
  });
});

describe("compatibility", () => {
  const task = "0190a5a0-0000-7000-8000-000000000701";

  /** A sync that is expected to be refused, so the status and facts can be read. */
  async function refuse(
    token: string,
    body: SyncRequest,
  ): Promise<{ status: number; code?: string; log_id?: string; head?: number; protocol?: number; schema?: number }> {
    const response = await call("/api/sync", { method: "POST", token, body });
    const facts = (await response.json()) as Record<string, unknown>;
    return { ...facts, status: response.status };
  }

  it("names its log, and serves a database that has no position yet", async () => {
    const account = await register("nameless@example.com");
    const pushed = await sync(
      account.token,
      "device-a",
      0,
      create(task, "task", [["name", { text: "mine" }]], ago(1000)),
    );
    expect(pushed.cursor.log).toMatch(/^[0-9a-f-]{36}$/);

    const adopted = await sync(account.token, "device-b", 0);
    expect(adopted.cursor.log).toBe(pushed.cursor.log);
    // No position means no history yet, so the whole log comes back.
    expect(adopted.events.map((row) => row.field).sort()).toEqual(["", "name", "start_time"]);
  });

  it("refuses a cursor minted by another log, and appends nothing", async () => {
    const account = await register("elsewhere@example.com");
    const learned = await sync(account.token, "device-a", 0);
    const mine = learned.cursor.log;

    const refused = await refuse(account.token, {
      ...request(account.token, "device-a", 0, create(task, "task", [["name", { text: "x" }]], ago(1000))),
      cursor: { log: "the-other-deployment", version: 0 },
    });
    expect(refused.status).toBe(409);
    expect(refused.code).toBe("log_mismatch");
    expect(refused.log_id).toBe(mine);
    expect(refused.head).toBe(0);

    // Refused before the append, so the push left no trace.
    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.events).toEqual([]);
  });

  it("refuses a cursor past the end of the log", async () => {
    const account = await register("rewound@example.com");
    const pushed = await sync(
      account.token,
      "device-a",
      0,
      create(task, "task", [["name", { text: "mine" }]], ago(1000)),
    );

    // A log restored from an older copy: same identity, fewer versions. The
    // device's cursor is not a position in it any more.
    const refused = await refuse(account.token, {
      ...request(account.token, "device-a", pushed.cursor.version + 1, []),
    });
    expect(refused.status).toBe(409);
    expect(refused.code).toBe("cursor_ahead");
    expect(refused.head).toBe(pushed.cursor.version);
  });

  it("refuses a peer that speaks another protocol, and names both numbers", async () => {
    const account = await register("skewed@example.com");
    const body = request(account.token, "device-a", 0, []);
    const refused = await refuse(account.token, { ...body, protocol: PROTOCOL + 1 });
    expect(refused.status).toBe(409);
    expect(refused.code).toBe("version_mismatch");
    expect(refused.protocol).toBe(PROTOCOL);
    expect(refused.schema).toBe(SCHEMA_VERSION);
  });

  it("refuses a peer that speaks another cell vocabulary", async () => {
    const account = await register("vocabulary@example.com");
    const body = request(account.token, "device-a", 0, []);
    const refused = await refuse(account.token, { ...body, schema: SCHEMA_VERSION + 1 });
    expect(refused.status).toBe(409);
    expect(refused.code).toBe("version_mismatch");
    expect(refused.schema).toBe(SCHEMA_VERSION);
  });

  it("rejects a cursor that is not a position at all", async () => {
    const account = await register("malformed@example.com");
    const body = request(account.token, "device-a", 0, []);
    const refused = await refuse(account.token, {
      ...body,
      cursor: { version: 0 } as unknown as SyncRequest["cursor"],
    });
    expect(refused.status).toBe(400);
    // A version with no name is not a position.
    const numeric = await refuse(account.token, {
      ...body,
      cursor: 0 as unknown as SyncRequest["cursor"],
    });
    expect(numeric.status).toBe(400);
  });
});
