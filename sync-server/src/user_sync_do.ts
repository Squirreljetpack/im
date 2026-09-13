/**
 * The per-user Durable Object: an append-only event log.
 *
 * A sync request appends the client's events and returns everything the client
 * has not seen yet, in pages — events from the client's own device are
 * excluded, so a device never reads back its own echo (§5.1). `event_id` is
 * UNIQUE, so a retried push is a no-op instead of a duplicate.
 */

import type {
  Env,
  LogRow,
  RemoteEvent,
  SyncEvent,
  SyncRequest,
  SyncResponse,
} from "./types";
import { error, json } from "./types";

/** How many events one pull returns before the client has to ask again. */
export const PAGE_LIMIT = 1000;

export class UserSyncDO implements DurableObject {
  private readonly ctx: DurableObjectState;

  constructor(ctx: DurableObjectState, _env: Env) {
    this.ctx = ctx;
    ctx.blockConcurrencyWhile(async () => {
      const sql = ctx.storage.sql;
      sql.exec(
        `CREATE TABLE IF NOT EXISTS sync_log (
           version    INTEGER PRIMARY KEY AUTOINCREMENT,
           device_id  TEXT NOT NULL,
           event_id   TEXT NOT NULL UNIQUE,
           entity_id  TEXT NOT NULL,
           timestamp  INTEGER NOT NULL,
           payload    TEXT NOT NULL,
           created_at INTEGER NOT NULL
         )`,
      );
      sql.exec(
        `CREATE INDEX IF NOT EXISTS idx_sync_log_version_device
           ON sync_log(version, device_id)`,
      );
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/sync") && request.method === "POST") {
      return this.sync(request);
    }
    return error("not found", 404);
  }

  private async sync(request: Request): Promise<Response> {
    let body: SyncRequest;
    try {
      body = (await request.json()) as SyncRequest;
    } catch {
      return error("invalid json body", 400);
    }
    if (typeof body?.device_id !== "string" || body.device_id === "") {
      return error("device_id is required", 400);
    }
    const since = Number.isInteger(body.since_version) ? body.since_version : 0;
    const events = Array.isArray(body.client_events) ? body.client_events : [];

    const now = Math.floor(Date.now() / 1000);
    const response = this.ctx.storage.transactionSync(() => {
      const sql = this.ctx.storage.sql;
      for (const event of events) {
        if (!isSyncEvent(event)) {
          continue;
        }
        sql.exec(
          `INSERT OR IGNORE INTO sync_log
             (device_id, event_id, entity_id, timestamp, payload, created_at)
           VALUES (?, ?, ?, ?, ?, ?)`,
          event.device_id,
          event.event_id,
          event.entity_id,
          event.timestamp,
          JSON.stringify(event.payload ?? null),
          now,
        );
      }

      // SAFETY: the SELECT lists exactly the columns of `LogRow`.
      const rows = sql
        .exec(
          `SELECT version, event_id, device_id, entity_id, timestamp, payload, created_at
             FROM sync_log
            WHERE version > ? AND device_id != ?
            ORDER BY version ASC
            LIMIT ?`,
          since,
          body.device_id,
          PAGE_LIMIT,
        )
        .toArray() as unknown as LogRow[];
      const hasMore = rows.length === PAGE_LIMIT;
      // A full page stops at its last row; a short one means the log is
      // drained for this device, so the cursor can jump to its head.
      const last = rows[rows.length - 1];
      const newVersion = hasMore && last !== undefined ? last.version : headVersion(sql, since);

      return {
        new_server_version: newVersion,
        has_more: hasMore,
        remote_events: rows.map(toRemoteEvent),
      };
    });

    const payload: SyncResponse = response;
    return json(payload);
  }

}

/** The log's newest version, never below the client's own cursor. */
function headVersion(sql: SqlStorage, since: number): number {
  // SAFETY: the query selects one column, aliased `version`.
  const row = sql
    .exec(`SELECT COALESCE(MAX(version), ?) AS version FROM sync_log`, since)
    .one() as { version: number };
  return Number(row.version);
}

function isSyncEvent(event: unknown): event is SyncEvent {
  if (typeof event !== "object" || event === null) {
    return false;
  }
  const candidate = event as Partial<SyncEvent>;
  return (
    typeof candidate.event_id === "string" &&
    typeof candidate.entity_id === "string" &&
    typeof candidate.device_id === "string" &&
    typeof candidate.timestamp === "number"
  );
}

function toRemoteEvent(row: LogRow): RemoteEvent {
  return {
    version: Number(row.version),
    event_id: row.event_id,
    entity_id: row.entity_id,
    device_id: row.device_id,
    timestamp: Number(row.timestamp),
    payload: JSON.parse(row.payload),
  };
}
