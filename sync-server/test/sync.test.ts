import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { PAGE_LIMIT } from "../src/user_sync_do";
import type { AuthResponse, SyncResponse } from "../src/types";

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

function event(
  eventId: string,
  entityId: string,
  timestamp: number,
  payload: unknown = null,
  device = "device-a",
) {
  return { event_id: eventId, entity_id: entityId, device_id: device, timestamp, payload };
}

async function sync(
  token: string,
  device: string,
  sinceVersion: number,
  clientEvents: ReturnType<typeof event>[] = [],
): Promise<SyncResponse> {
  const response = await call("/api/v1/sync", {
    method: "POST",
    token,
    body: { device_id: device, since_version: sinceVersion, client_events: clientEvents },
  });
  expect(response.status).toBe(200);
  return (await response.json()) as SyncResponse;
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
    const session = (await login.json()) as AuthResponse;
    expect(session.user_id).toBe(account.user_id);

    const status = await call("/api/v1/auth/status", { token: session.token });
    expect(status.status).toBe(200);
    expect(await status.json()).toEqual({
      user_id: account.user_id,
      email: "alice@example.com",
    });
  });

  it("rejects duplicate emails, weak passwords and bad credentials", async () => {
    await register("bob@example.com");

    const duplicate = await call("/api/v1/auth/register", {
      method: "POST",
      body: { email: "bob@example.com", password: "correct horse battery" },
    });
    expect(duplicate.status).toBe(409);

    const weak = await call("/api/v1/auth/register", {
      method: "POST",
      body: { email: "weak@example.com", password: "short" },
    });
    expect(weak.status).toBe(400);

    const invalidEmail = await call("/api/v1/auth/register", {
      method: "POST",
      body: { email: "not-an-email", password: "correct horse battery" },
    });
    expect(invalidEmail.status).toBe(400);

    const wrongPassword = await call("/api/v1/auth/login", {
      method: "POST",
      body: { email: "bob@example.com", password: "wrong horse battery" },
    });
    expect(wrongPassword.status).toBe(401);
  });

  it("requires a token for status and sync", async () => {
    expect((await call("/api/v1/auth/status")).status).toBe(401);
    expect((await call("/api/v1/sync", { method: "POST", body: {} })).status).toBe(401);
    expect(
      (await call("/api/v1/auth/status", { token: "not-a-token" })).status,
    ).toBe(401);
  });
});

describe("event log", () => {
  it("appends events and hands them to the other device only", async () => {
    const account = await register("carol@example.com");
    const entity = "0193f000-0000-7000-8000-000000000001";

    const pushed = await sync(account.token, "device-a", 0, [
      event("evt-1", entity, 1_000, { type: "Task", data: { name: "write it" } }),
    ]);
    expect(pushed.new_server_version).toBe(1);
    expect(pushed.remote_events).toEqual([]);

    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.new_server_version).toBe(1);
    expect(pulled.remote_events).toHaveLength(1);
    expect(pulled.remote_events[0]).toMatchObject({
      version: 1,
      event_id: "evt-1",
      device_id: "device-a",
      entity_id: entity,
      timestamp: 1_000,
      payload: { type: "Task", data: { name: "write it" } },
    });

    // The pushing device does not see its own event echoed back.
    const own = await sync(account.token, "device-a", 0);
    expect(own.remote_events).toEqual([]);
  });

  it("is idempotent for retried pushes", async () => {
    const account = await register("dave@example.com");
    const entity = "0193f000-0000-7000-8000-000000000002";
    const retry = [event("evt-retry", entity, 2_000, null)];

    const first = await sync(account.token, "device-a", 0, retry);
    expect(first.new_server_version).toBe(1);

    const second = await sync(account.token, "device-a", 0, retry);
    expect(second.new_server_version).toBe(1);

    const pulled = await sync(account.token, "device-b", 0);
    expect(pulled.remote_events).toHaveLength(1);
  });

  it("replays both devices' events in log order", async () => {
    const account = await register("erin@example.com");
    const entity = "0193f000-0000-7000-8000-000000000003";

    await sync(account.token, "device-a", 0, [event("evt-a", entity, 1_000, null)]);
    await sync(account.token, "device-b", 0, [event("evt-b", entity, 2_000, null)]);
    await sync(account.token, "device-a", 0, [event("evt-c", entity, 3_000, null)]);

    const pulled = await sync(account.token, "device-c", 0);
    expect(pulled.remote_events.map((row) => row.event_id)).toEqual([
      "evt-a",
      "evt-b",
      "evt-c",
    ]);
    expect(pulled.remote_events.map((row) => row.version)).toEqual([1, 2, 3]);
    expect(pulled.new_server_version).toBe(3);

    // A cursor past the first event returns only what follows.
    const delta = await sync(account.token, "device-c", 2);
    expect(delta.remote_events.map((row) => row.event_id)).toEqual(["evt-c"]);
  });

  it("keeps users isolated", async () => {
    const alice = await register("frank@example.com");
    const bob = await register("grace@example.com");
    const entity = "0193f000-0000-7000-8000-000000000004";

    await sync(alice.token, "device-a", 0, [event("evt-private", entity, 1_000, null)]);

    const pulled = await sync(bob.token, "device-b", 0);
    expect(pulled.new_server_version).toBe(0);
    expect(pulled.remote_events).toEqual([]);
  });
});

describe("pagination", () => {
  it("pages a backlog and reports when more is waiting", async () => {
    const account = await register("heidi@example.com");
    const entity = "0193f000-0000-7000-8000-000000000005";
    const backlog = Array.from({ length: PAGE_LIMIT + 1 }, (_, index) =>
      event(`evt-${index}`, entity, index, null, "device-a"),
    );
    await sync(account.token, "device-a", 0, backlog);

    const first = await sync(account.token, "device-b", 0);
    expect(first.remote_events).toHaveLength(PAGE_LIMIT);
    expect(first.has_more).toBe(true);
    expect(first.new_server_version).toBe(PAGE_LIMIT);

    const second = await sync(account.token, "device-b", first.new_server_version);
    expect(second.remote_events).toHaveLength(1);
    expect(second.has_more).toBe(false);
    expect(second.new_server_version).toBe(PAGE_LIMIT + 1);
  });

  it("drains a short page and jumps the cursor to the log head", async () => {
    const account = await register("ivan@example.com");
    const foreign = "0193f000-0000-7000-8000-000000000006";
    const own = "0193f000-0000-7000-8000-000000000007";
    await sync(account.token, "device-a", 0, [event("evt-foreign", foreign, 1_000, null)]);
    // This device's own event is never echoed back, but the cursor still
    // advances past it.
    await sync(account.token, "device-b", 0, [event("evt-own", own, 2_000, null, "device-b")]);

    const page = await sync(account.token, "device-b", 0);
    expect(page.remote_events.map((row) => row.event_id)).toEqual(["evt-foreign"]);
    expect(page.has_more).toBe(false);
    expect(page.new_server_version).toBe(2);
  });
});
