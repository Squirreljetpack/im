/**
 * Worker gateway: user accounts in D1, then every sync request is routed to
 * the caller's own Durable Object (`idFromName(user_id)`).
 */

import { authenticate, hashPassword, issueToken, verifyPassword } from "./auth";
import type { AuthResponse, Env, UserRow } from "./types";
import { error, json } from "./types";
import { UserSyncDO } from "./user_sync_do";

export { UserSyncDO };

const MIN_PASSWORD_LENGTH = 8;
const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    switch (path) {
      case "/api/v1/auth/register":
        return register(request, env);
      case "/api/v1/auth/login":
        return login(request, env);
      case "/api/v1/auth/status":
        return status(request, env);
      case "/api/v1/sync":
        return forwardToUser(request, env);
      default:
        return error("not found", 404);
    }
  },
} satisfies ExportedHandler<Env>;

/** Route an authenticated request to the caller's Durable Object. */
async function forwardToUser(request: Request, env: Env): Promise<Response> {
  const userId = await authenticate(request, env);
  if (userId === null) {
    return error("unauthorized", 401);
  }
  const stub = env.USER_DO.get(env.USER_DO.idFromName(userId));
  return stub.fetch(request);
}

async function register(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") {
    return error("method not allowed", 405);
  }
  const credentials = await readCredentials(request);
  if (typeof credentials === "string") {
    return error(credentials, 400);
  }
  const { email, password } = credentials;
  if (password.length < MIN_PASSWORD_LENGTH) {
    return error(`password must be at least ${MIN_PASSWORD_LENGTH} characters`, 400);
  }

  const existing = await findUser(env, email);
  if (existing !== undefined) {
    return error("email is already registered", 409);
  }

  const user: UserRow = {
    id: crypto.randomUUID(),
    email,
    password_hash: await hashPassword(password),
    created_at: Math.floor(Date.now() / 1000),
  };
  try {
    await env.AUTH_DB.prepare(
      "INSERT INTO users (id, email, password_hash, created_at) VALUES (?, ?, ?, ?)",
    )
      .bind(user.id, user.email, user.password_hash, user.created_at)
      .run();
  } catch (cause) {
    // A concurrent registration can still lose the UNIQUE race.
    if (isMissingTable(cause)) {
      return unmigratedDatabase();
    }
    return error("email is already registered", 409);
  }

  const response: AuthResponse = {
    user_id: user.id,
    email: user.email,
    token: await issueToken(user.id, env.JWT_SECRET, Math.floor(Date.now() / 1000)),
  };
  return json(response, 201);
}

async function login(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") {
    return error("method not allowed", 405);
  }
  const credentials = await readCredentials(request);
  if (typeof credentials === "string") {
    return error(credentials, 400);
  }
  const { email, password } = credentials;

  const user = await findUser(env, email);
  if (user === undefined || !(await verifyPassword(password, user.password_hash))) {
    return error("invalid email or password", 401);
  }

  const response: AuthResponse = {
    user_id: user.id,
    email: user.email,
    token: await issueToken(user.id, env.JWT_SECRET, Math.floor(Date.now() / 1000)),
  };
  return json(response);
}

async function status(request: Request, env: Env): Promise<Response> {
  if (request.method !== "GET") {
    return error("method not allowed", 405);
  }
  const userId = await authenticate(request, env);
  if (userId === null) {
    return error("unauthorized", 401);
  }
  const user = await env.AUTH_DB.prepare(
    "SELECT id, email FROM users WHERE id = ?",
  )
    .bind(userId)
    .first<{ id: string; email: string }>();
  if (user === null) {
    // The token outlived the account.
    return error("unknown user", 401);
  }
  return json({ user_id: user.id, email: user.email });
}

async function findUser(env: Env, email: string): Promise<UserRow | undefined> {
  try {
    const row = await env.AUTH_DB.prepare(
      "SELECT id, email, password_hash, created_at FROM users WHERE email = ?",
    )
      .bind(email)
      .first<UserRow>();
    return row ?? undefined;
  } catch (cause) {
    if (isMissingTable(cause)) {
      throw new Error(unmigratedDatabaseMessage());
    }
    throw cause;
  }
}

async function readCredentials(
  request: Request,
): Promise<{ email: string; password: string } | string> {
  let body: unknown;
  try {
    body = await request.json();
  } catch {
    return "invalid json body";
  }
  if (typeof body !== "object" || body === null) {
    return "expected a json object";
  }
  const { email, password } = body as { email?: unknown; password?: unknown };
  if (typeof email !== "string" || !EMAIL_PATTERN.test(email)) {
    return "a valid email is required";
  }
  if (typeof password !== "string" || password === "") {
    return "a password is required";
  }
  return { email: email.trim().toLowerCase(), password };
}

function unmigratedDatabaseMessage(): string {
  return "the auth database is not migrated: run `wrangler d1 migrations apply AUTH_DB --local`";
}

function unmigratedDatabase(): Response {
  return error(unmigratedDatabaseMessage(), 500);
}

function isMissingTable(cause: unknown): boolean {
  return cause instanceof Error && /no such table/i.test(cause.message);
}
