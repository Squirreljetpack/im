/**
 * The per-user Durable Object: an append-only event log plus a WebSocket hub.
 *
 * A sync request appends the client's events and returns everything the client
 * has not seen yet — including events from the client's own device, which the
 * caller filters out by `device_id` (§5.1). `event_id` is UNIQUE, so a retried
 * push is a no-op instead of a duplicate.
 */

import type {
  ClientEvent,
  Env,
  LogRow,
  RemoteEvent,
  SyncRequest,
  SyncResponse,
} from "./types";
import { error, json } from "./types";

interface Attachment {
  device_id: string;
}

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
    if (url.pathname.endsWith("/sync/ws")) {
      return this.openSocket(request);
    }
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
      let appended = 0;
      for (const event of events) {
        if (!isClientEvent(event)) {
          continue;
        }
        const cursor = sql.exec(
          `INSERT OR IGNORE INTO sync_log
             (device_id, event_id, entity_id, timestamp, payload, created_at)
           VALUES (?, ?, ?, ?, ?, ?)`,
          body.device_id,
          event.event_id,
          event.id,
          event.timestamp,
          JSON.stringify(event.payload ?? null),
          now,
        );
        appended += Number(cursor.rowsWritten ?? 0);
      }

      const rows = sql
        .exec(
          `SELECT version, event_id, device_id, entity_id, timestamp, payload
             FROM sync_log
            WHERE version > ? AND device_id != ?
            ORDER BY version ASC`,
          since,
          body.device_id,
        )
        .toArray() as unknown as LogRow[];
      const head = sql
        .exec(
          `SELECT COALESCE(MAX(version), ?) AS version FROM sync_log`,
          since,
        )
        .one() as unknown as { version: number };

      return {
        appended,
        new_server_version: Number(head.version),
        remote_events: rows.map(toRemoteEvent),
      };
    });

    if (response.appended > 0) {
      this.broadcast(body.device_id, response.new_server_version);
    }
    const payload: SyncResponse = {
      new_server_version: response.new_server_version,
      remote_events: response.remote_events,
    };
    return json(payload);
  }

  private openSocket(request: Request): Response {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return error("expected a websocket upgrade", 426);
    }
    const device = new URL(request.url).searchParams.get("device_id") ?? "";
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server);
    server.serializeAttachment({ device_id: device } satisfies Attachment);
    return new Response(null, { status: 101, webSocket: client });
  }

  /** Tell every other connected device that the log moved on. */
  private broadcast(originDevice: string, version: number): void {
    const message = JSON.stringify({ type: "new_version", version });
    for (const socket of this.ctx.getWebSockets()) {
      const attachment = socket.deserializeAttachment() as Attachment | null;
      if (attachment?.device_id === originDevice) {
        continue;
      }
      try {
        socket.send(message);
      } catch {
        // A socket that died mid-send is dropped by the runtime.
      }
    }
  }

  async webSocketMessage(_socket: WebSocket, _message: string | ArrayBuffer) {
    // The hub is push-only: clients push through POST /api/v1/sync.
  }

  async webSocketClose(socket: WebSocket) {
    socket.close();
  }

  async webSocketError(socket: WebSocket) {
    socket.close();
  }
}

function isClientEvent(event: unknown): event is ClientEvent {
  if (typeof event !== "object" || event === null) {
    return false;
  }
  const candidate = event as Partial<ClientEvent>;
  return (
    typeof candidate.event_id === "string" &&
    typeof candidate.id === "string" &&
    typeof candidate.timestamp === "number"
  );
}

function toRemoteEvent(row: LogRow): RemoteEvent {
  return {
    version: Number(row.version),
    event_id: row.event_id,
    device_id: row.device_id,
    id: row.entity_id,
    timestamp: Number(row.timestamp),
    payload: JSON.parse(row.payload),
  };
}
