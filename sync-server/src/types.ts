/** Bindings and the wire format shared by the Worker, the DO and the client. */

export interface Env {
  USER_DO: DurableObjectNamespace;
  AUTH_DB: D1Database;
  JWT_SECRET: string;
}

/** The reserved field that carries an entity's existence. */
export const EXISTENCE = "";

/**
 * One cell value with its SQLite storage class kept explicit: a tracker score
 * of `3` and `3.0` are different rows in a dynamically typed column, and
 * JavaScript numbers alone cannot tell them apart.
 */
export type CellValue = { int: number } | { real: number } | { text: string };

/** One entity kind. The field inventory lives in `sync_schema.json`. */
export type Kind = "task" | "mood" | "tracker" | "completion";

/**
 * One write to one cell: a create is the existence cell plus a burst of these,
 * an edit is one of them, a delete is the existence cell written `deleted`.
 */
export interface CellEvent {
  event_id: string;
  device_id: string;
  entity_id: string;
  kind: string;
  field: string;
  value: CellValue | null;
  /** The author's stamp: decides which write owns the cell. Milliseconds. */
  timestamp: number;
}

/** A log row: the event plus the position the server gave it. */
export interface LogRow extends CellEvent {
  /** Append order; decides delivery only, never who wins a cell. */
  version: number;
}

/** `POST /api/sync` request body. */
export interface SyncRequest {
  device_id: string;
  /** Highest `winner.version` the client has merged. */
  cursor: number;
  events: CellEvent[];
}

export type VerdictOutcome = "accepted" | "superseded" | "rejected";

export type RejectReason =
  | "shape"
  | "device"
  | "kind"
  | "field"
  | "class"
  | "deleted"
  | "dangling"
  | "self_parent"
  | "kind_change"
  | "immutable"
  | "clock_ahead";

export interface Verdict {
  event_id: string;
  outcome: VerdictOutcome;
  reason?: RejectReason;
  /** The register's winning event for the targeted cell, if it has one. */
  winner?: LogRow | null;
  /** The entity's existence cell, when the entity is gone. */
  tombstone?: LogRow | null;
}

export interface SyncResponse {
  /** Advance the client's cursor to this. */
  cursor: number;
  /** The server's clock, for diagnosing `clock_ahead`. */
  now: number;
  has_more: boolean;
  events: LogRow[];
  verdicts: Verdict[];
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
