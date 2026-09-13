/** Bindings and the wire format shared by the Worker, the DO and the client. */

export interface Env {
  USER_DO: DurableObjectNamespace;
  AUTH_DB: D1Database;
  JWT_SECRET: string;
}

/** A row of the Durable Object's append-only log. */
export interface LogRow {
  version: number;
  device_id: string;
  event_id: string;
  entity_id: string;
  timestamp: number;
  payload: string;
}

/**
 * One entity event: `payload` is the client's opaque `Option<EntityPayload>`
 * (`null` = delete). The server never interprets it.
 */
export interface ClientEvent {
  event_id: string;
  id: string;
  timestamp: number;
  payload: unknown;
}

/** `POST /api/v1/sync` request body. */
export interface SyncRequest {
  device_id: string;
  since_version: number;
  client_events: ClientEvent[];
}

/** One event handed back to the client, with the position it occupies. */
export interface RemoteEvent {
  version: number;
  event_id: string;
  device_id: string;
  id: string;
  timestamp: number;
  payload: unknown;
}

export interface SyncResponse {
  new_server_version: number;
  remote_events: RemoteEvent[];
}

export interface UserRow {
  id: string;
  email: string;
  password_hash: string;
  created_at: number;
}

export interface AuthResponse {
  user_id: string;
  email: string;
  token: string;
}

export function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export function error(message: string, status: number): Response {
  return json({ error: message }, status);
}
