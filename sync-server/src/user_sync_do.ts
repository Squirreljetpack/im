/**
 * The per-user Durable Object: an append-only cell log plus a materialised
 * register that names the winning event of every `(entity_id, field)` cell.
 *
 * A push is validated cell by cell inside one transaction — nothing is ever
 * appended and then retracted — and every pushed event gets a verdict. A pull
 * is a register diff: cells whose winner sits above the client's cursor, with
 * the client's own cells suppressed unless it is materialising from nothing.
 *
 * The register is a pure fold of the log (see `fold`), so it is a cache that
 * can be rebuilt, never a second truth.
 */

import {
  classMatches,
  decode,
  encode,
  fieldSpec,
  isCellValue,
  kindSpec,
  subkindFrom,
} from "./schema";
import { EXISTENCE, error, json } from "./types";
import type {
  CellEvent,
  Env,
  LogRow,
  RejectReason,
  SupersededNotice,
  SyncRequest,
  SyncResponse,
  Verdict,
} from "./types";

/** Stamps further than this ahead of the server's clock are refused. */
const SKEW_MS = 120_000;
/** Cells in one pull. A steady-state delta is tiny; this only bounds a reset. */
export const PAGE_LIMIT = 5000;
/** Events in one push, so one transaction cannot grow without bound. */
const MAX_PUSH = 1000;

const LIVE = encode({ text: "live" });
const DEAD = encode({ text: "deleted" });

interface CellRow {
  entity_id: string;
  field: string;
  kind: string;
  value: string | null;
  timestamp: number;
  device_id: string;
  event_id: string;
  winner_version: number;
}

type Sql = SqlStorage;

export class UserSyncDO implements DurableObject {
  private readonly ctx: DurableObjectState;

  constructor(ctx: DurableObjectState, _env: Env) {
    this.ctx = ctx;
    ctx.blockConcurrencyWhile(async () => {
      this.createTables(ctx.storage.sql);
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/sync") && request.method === "POST") {
      return this.sync(request, pageSize(url));
    }
    return error("not found", 404);
  }

  private createTables(sql: Sql): void {
    // A log written before cells existed stores opaque payloads that cannot be
    // folded into cells, so this storage starts over. That is only safe
    // because the server has never been deployed; a live instance would need a
    // payload -> cells backfill instead, or its clients could not rebuild.
    const columns = sql
      .exec("SELECT name FROM pragma_table_info('sync_log')")
      .toArray() as unknown as { name: string }[];
    if (
      columns.length > 0 &&
      !columns.some((column) => column.name === "kind" || column.name === "batch")
    ) {
      sql.exec("DROP TABLE IF EXISTS sync_log");
      sql.exec("DROP TABLE IF EXISTS sync_register");
    }

    sql.exec(
      `CREATE TABLE IF NOT EXISTS sync_log (
         version    INTEGER PRIMARY KEY AUTOINCREMENT,
         event_id   TEXT NOT NULL UNIQUE,
         device_id  TEXT NOT NULL,
         entity_id  TEXT NOT NULL,
         kind       TEXT NOT NULL,
         field      TEXT NOT NULL,
         value      TEXT,
         timestamp  INTEGER NOT NULL,
         created_at INTEGER NOT NULL,
         batch      INTEGER NOT NULL
       )`,
    );
    sql.exec(
      `CREATE TABLE IF NOT EXISTS sync_register (
         entity_id      TEXT NOT NULL,
         field          TEXT NOT NULL,
         kind           TEXT NOT NULL,
         value          TEXT,
         timestamp      INTEGER NOT NULL,
         device_id      TEXT NOT NULL,
         event_id       TEXT NOT NULL,
         winner_version INTEGER NOT NULL,
         PRIMARY KEY (entity_id, field)
       )`,
    );
    // The pull window scans by winner, and the cascade synthesis looks cells up
    // by the value they hold (`WHERE field = ? AND value = ?`).
    sql.exec(
      `CREATE INDEX IF NOT EXISTS idx_sync_register_version
         ON sync_register(winner_version)`,
    );
    sql.exec(
      `CREATE INDEX IF NOT EXISTS idx_sync_register_link
         ON sync_register(field, value)`,
    );
    // A page may only break between pushes, never inside one, so it needs the
    // batch of a winning version.
    sql.exec(
      `CREATE INDEX IF NOT EXISTS idx_sync_log_batch ON sync_log(batch)`,
    );
    // Cells that a later event of another device took away from their author.
    // Delivered with the pull that carries the winner and pruned once that
    // device's cursor has passed it, so a lost response re-delivers instead of
    // losing the explanation.
    sql.exec(
      `CREATE TABLE IF NOT EXISTS sync_superseded (
         device_id      TEXT NOT NULL,
         event_id       TEXT NOT NULL,
         entity_id      TEXT NOT NULL,
         field          TEXT NOT NULL,
         lost           TEXT,
         winner         TEXT NOT NULL,
         winner_version INTEGER NOT NULL,
         created_at     INTEGER NOT NULL,
         PRIMARY KEY (device_id, event_id)
       )`,
    );
  }

  private async sync(request: Request, pageSize: number): Promise<Response> {
    let body: SyncRequest;
    try {
      body = (await request.json()) as SyncRequest;
    } catch {
      return error("invalid json body", 400);
    }
    if (typeof body?.device_id !== "string" || body.device_id === "") {
      return error("device_id is required", 400);
    }
    if (!Array.isArray(body.events)) {
      return error("events must be an array", 400);
    }
    if (body.events.length > MAX_PUSH) {
      return error(`at most ${MAX_PUSH} events per push`, 400);
    }
    const device = body.device_id;
    const cursor = Number.isInteger(body.cursor) && body.cursor > 0 ? body.cursor : 0;
    const now = Date.now();

    const payload: SyncResponse = this.ctx.storage.transactionSync(() => {
      const sql = this.ctx.storage.sql;
      // Everything appended by this push shares one batch: a page may break
      // between pushes but never inside one, which is what keeps a create's
      // cells from arriving in two different responses.
      // SAFETY: the query selects one column, aliased `batch`.
      const batch = Number(
        (sql
          .exec("SELECT COALESCE(MAX(batch), 0) + 1 AS batch FROM sync_log")
          .one() as { batch: number }).batch,
      );
      const verdicts: (Verdict | null)[] = body.events.map(() => null);
      const pushed: { index: number; entity_id: string; field: string; event_id: string }[] = [];
      // Entities created by this push: their field writes are part of the
      // create, so they may legitimately establish the task kind.
      const creating = new Set<string>();

      body.events.forEach((raw, index) => {
        const result = this.push(sql, raw, device, now, creating, batch);
        if (result.verdict.outcome === "rejected") {
          verdicts[index] = result.verdict;
        } else {
          pushed.push({ index, ...result.cell });
        }
      });
      // A cell is only accepted or superseded once the whole batch has landed:
      // a later event of the same push can legitimately take the cell away
      // from an earlier one.
      for (const event of pushed) {
        verdicts[event.index] = this.settle(sql, event);
      }

      const pull = this.pull(sql, cursor, device, pageSize);
      return {
        cursor: pull.cursor,
        now,
        has_more: pull.hasMore,
        events: pull.rows,
        verdicts: verdicts.filter((verdict): verdict is Verdict => verdict !== null),
        superseded: this.notices(sql, cursor, device),
      };
    });

    return json(payload);
  }

  /**
   * Validate one pushed cell and append it. Returns a rejection, or the cell
   * identity whose accepted/superseded verdict the batch resolves afterwards.
   */
  private push(
    sql: Sql,
    raw: unknown,
    device: string,
    now: number,
    creating: Set<string>,
    batch: number,
  ): {
    verdict: Verdict;
    cell: { entity_id: string; field: string; event_id: string };
  } {
    const candidate = raw as Partial<CellEvent> | null;
    const fallbackId = typeof candidate?.event_id === "string" ? candidate.event_id : "";
    if (!isCellEvent(candidate)) {
      return {
        verdict: { event_id: fallbackId, outcome: "rejected", reason: "shape" },
        cell: { entity_id: "", field: "", event_id: fallbackId },
      };
    }
    const event = candidate;
    const reject = (reason: RejectReason, withCells = true): ReturnType<typeof this.push> => ({
      verdict: {
        event_id: event.event_id,
        outcome: "rejected",
        reason,
        ...(withCells ? this.context(sql, event) : {}),
      },
      cell: { entity_id: event.entity_id, field: event.field, event_id: event.event_id },
    });

    if (event.device_id !== device) {
      return reject("device", false);
    }
    const spec = kindSpec(event.kind);
    if (spec === undefined) {
      return reject("kind", false);
    }
    if (event.field !== EXISTENCE && !Object.hasOwn(spec.fields, event.field)) {
      return reject("field", false);
    }
    // A retried push is a no-op: the log's copy is authoritative and the
    // verdict is re-derived from the register.
    if (this.logged(sql, event.event_id)) {
      return {
        verdict: { event_id: event.event_id, outcome: "accepted" },
        cell: { entity_id: event.entity_id, field: event.field, event_id: event.event_id },
      };
    }
    if (!Number.isInteger(event.timestamp) || event.timestamp < 0) {
      return reject("shape", false);
    }
    if (event.timestamp > now + SKEW_MS) {
      return reject("clock_ahead", false);
    }
    if (event.value !== null && !isCellValue(event.value)) {
      return reject("class", false);
    }

    const existence = this.cell(sql, event.entity_id, EXISTENCE);
    const dead = existence?.value === DEAD;
    if (existence !== undefined && existence.kind !== event.kind) {
      return reject("kind");
    }

    if (event.field === EXISTENCE) {
      const marker = event.value !== null && "text" in event.value ? event.value.text : null;
      if (marker !== "live" && marker !== "deleted") {
        return reject("class", false);
      }
      if (marker === "live" && dead) {
        return reject("deleted");
      }
      const appended = this.append(sql, event, now, batch);
      if (marker === "live") {
        creating.add(event.entity_id);
      }
      if (marker === "deleted" && appended.won) {
        this.cascade(sql, event.kind, event.entity_id, event.timestamp, now, batch);
      }
      return {
        verdict: { event_id: event.event_id, outcome: "accepted" },
        cell: { entity_id: event.entity_id, field: EXISTENCE, event_id: event.event_id },
      };
    }

    if (existence === undefined) {
      // Every value write names an entity this device must already have, and a
      // create pushes its existence cell first.
      return reject("dangling");
    }
    if (dead) {
      return reject("deleted");
    }

    const field = fieldSpec(event.kind, event.field);
    if (field === undefined) {
      return reject("field", false);
    }
    if (event.value === null) {
      if (field.required === true) {
        return reject("class", false);
      }
    } else if (!classMatches(field.class, event.value)) {
      return reject("class", false);
    }
    if (field.immutable === true && this.cell(sql, event.entity_id, event.field) !== undefined) {
      return reject("immutable");
    }
    // A tracker's `time` decides which slot the row occupies, so it is held to
    // the same clock rule as a stamp: the number that decides data loss must
    // not be the unvalidated one.
    if (
      event.kind === "tracker" &&
      event.field === "time" &&
      event.value !== null &&
      "int" in event.value &&
      event.value.int > Math.floor((now + SKEW_MS) / 1000)
    ) {
      return reject("clock_ahead", false);
    }
    if (field.link !== undefined) {
      const referent = event.value !== null && "text" in event.value ? event.value.text : null;
      if (referent === null) {
        return reject("class", false);
      }
      if (event.field === "parent_id" && referent === event.entity_id) {
        return reject("self_parent", false);
      }
      const target = this.cell(sql, referent, EXISTENCE);
      if (target === undefined || target.kind !== field.link) {
        return reject("dangling");
      }
      if (target.value === DEAD) {
        return reject("deleted");
      }
    }
    if (
      spec.subkind !== undefined &&
      !creating.has(event.entity_id) &&
      spec.subkind.from.some(([name]) => name === event.field)
    ) {
      const rule = spec.subkind;
      const before = subkindFrom(rule, (name) => this.present(sql, event.entity_id, name));
      const after = subkindFrom(rule, (name) =>
        name === event.field ? event.value !== null : this.present(sql, event.entity_id, name),
      );
      if (before !== after) {
        // Kind is fixed at creation: a write may update a defining field but
        // may never introduce one or clear one.
        return reject("kind_change");
      }
    }

    this.append(sql, event, now, batch);
    return {
      verdict: { event_id: event.event_id, outcome: "accepted" },
      cell: { entity_id: event.entity_id, field: event.field, event_id: event.event_id },
    };
  }

  /**
   * Append a cell to the log and fold it into the register.
   *
   * `quiet` is for cells the server writes itself (the cascade): those are
   * repairs, not edits, so the device that loses a link to one is not told its
   * value was replaced — it is told the entity was deleted, which is why.
   */
  private append(
    sql: Sql,
    event: CellEvent,
    now: number,
    batch: number,
    quiet = false,
  ): { version: number; won: boolean } {
    const before = this.cell(sql, event.entity_id, event.field);
    const value = encode(event.value);
    sql.exec(
      `INSERT INTO sync_log
         (event_id, device_id, entity_id, kind, field, value, timestamp, created_at, batch)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      event.event_id,
      event.device_id,
      event.entity_id,
      event.kind,
      event.field,
      value,
      event.timestamp,
      now,
      batch,
    );
    // SAFETY: the query selects one column, aliased `version`.
    const row = sql.exec("SELECT last_insert_rowid() AS version").one() as {
      version: number;
    };
    const version = Number(row.version);
    this.fold(sql, event, version, value);
    const winner = this.cell(sql, event.entity_id, event.field);
    const won = winner?.event_id === event.event_id;
    // A write that takes a cell away from another device is the only way a
    // device can lose an edit it already pushed: it needs to be told, or it
    // watches its value change with no explanation.
    if (!quiet && won && before !== undefined && before.device_id !== event.device_id) {
      const winnerRow: LogRow = { ...event, version };
      sql.exec(
        `INSERT OR IGNORE INTO sync_superseded
           (device_id, event_id, entity_id, field, lost, winner, winner_version, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
        before.device_id,
        before.event_id,
        event.entity_id,
        event.field,
        before.value,
        JSON.stringify(winnerRow),
        version,
        now,
      );
    }
    return { version, won };
  }

  /**
   * The register is a max-join: the highest stamp wins the cell. Stamps are
   * compared as `(timestamp, device_id, event_id)`, so two devices that wrote
   * in the same millisecond still converge on one winner. `version` is
   * deliberately not compared — it is append order, and a past-dated push
   * arrives late carrying an old stamp.
   */
  private fold(sql: Sql, event: CellEvent, version: number, value: string | null): void {
    sql.exec(
      `INSERT INTO sync_register
         (entity_id, field, kind, value, timestamp, device_id, event_id, winner_version)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(entity_id, field) DO UPDATE SET
         kind           = excluded.kind,
         value          = excluded.value,
         timestamp      = excluded.timestamp,
         device_id      = excluded.device_id,
         event_id       = excluded.event_id,
         winner_version = excluded.winner_version
       WHERE (excluded.timestamp, excluded.device_id, excluded.event_id)
           > (sync_register.timestamp, sync_register.device_id, sync_register.event_id)`,
      event.entity_id,
      event.field,
      event.kind,
      value,
      event.timestamp,
      event.device_id,
      event.event_id,
      version,
    );
  }

  /**
   * Write the cells a delete forces on its neighbours, so the register agrees
   * with what every client's own foreign-key cascade does. Each synthesised
   * cell is stamped just above the *stale* link it replaces, so it beats that
   * link and loses to anything a user decides later.
   */
  private cascade(
    sql: Sql,
    kind: string,
    entityId: string,
    deletedAt: number,
    now: number,
    batch: number,
  ): void {
    const spec = kindSpec(kind);
    if (spec?.cascade === undefined) {
      return;
    }
    const link = encode({ text: entityId });
    for (const edge of spec.cascade) {
      if (edge.action === "delete") {
        const doomed = sql
          .exec(
            `SELECT entity_id FROM sync_register
              WHERE kind = ? AND field = ? AND value = ?
                AND entity_id NOT IN (
                  SELECT entity_id FROM sync_register
                   WHERE field = ? AND value = ?
                )`,
            edge.kind,
            "todo_id",
            link,
            EXISTENCE,
            DEAD,
          )
          .toArray() as unknown as { entity_id: string }[];
        for (const row of doomed) {
          const existence = this.cell(sql, row.entity_id, EXISTENCE);
          this.append(
            sql,
            {
              event_id: crypto.randomUUID(),
              device_id: existence?.device_id ?? this.deviceOf(sql, entityId),
              entity_id: row.entity_id,
              kind: edge.kind,
              field: EXISTENCE,
              value: { text: "deleted" },
              timestamp: Math.max(deletedAt, existence?.timestamp ?? 0) + 1,
            },
            now,
            batch,
            true,
          );
        }
        continue;
      }
      const stale = sql
        .exec(
          `SELECT entity_id, timestamp FROM sync_register
            WHERE kind = ? AND field = ? AND value = ?`,
          edge.kind,
          edge.field,
          link,
        )
        .toArray() as unknown as { entity_id: string; timestamp: number }[];
      for (const row of stale) {
        this.append(
          sql,
          {
            event_id: crypto.randomUUID(),
            device_id: this.deviceOf(sql, entityId),
            entity_id: row.entity_id,
            kind: edge.kind,
            field: edge.field ?? "",
            value: null,
            timestamp: Math.max(deletedAt, Number(row.timestamp)) + 1,
          },
          now,
          batch,
          true,
        );
      }
    }
  }

  /** The device that authored the deleting cell, so the repair is attributable. */
  private deviceOf(sql: Sql, entityId: string): string {
    const existence = this.cell(sql, entityId, EXISTENCE);
    return existence?.device_id ?? "";
  }

  /**
   * A pull is a register diff over `(cursor, head]`.
   *
   * The cursor is the last winner version delivered, which is a safe page key
   * because a cell's `winner_version` only moves forward: a write that loses
   * leaves it untouched, and one that wins takes the new append's version.
   *
   * A page may only break *between* pushes. A create writes its existence cell
   * and its fields as separate rows, so a boundary inside one push would hand a
   * client a row's existence without the `NOT NULL` fields that make it
   * insertable — the row would be skipped and then lost, since the cursor moves
   * past it. Everything a push appends shares one batch, so a full page is
   * extended to the end of its last batch.
   */
  private pull(
    sql: Sql,
    cursor: number,
    device: string,
    limit: number,
  ): { rows: LogRow[]; cursor: number; hasMore: boolean } {
    // Own-authored cells are skipped because the client's rows already reflect
    // them — except at cursor 0, where there is no row that could.
    // A dead entity is served as its existence cell alone: nothing may write a
    // value into a row that no longer exists.
    const query = (batchBound: boolean): string =>
      `SELECT r.entity_id, r.field, r.kind, r.value, r.timestamp, r.device_id,
              r.event_id, r.winner_version
         FROM sync_register r
        WHERE r.winner_version > ?
          AND (? = 0 OR r.device_id != ?)
          AND (r.field = ?
               OR NOT EXISTS (SELECT 1 FROM sync_register e
                               WHERE e.entity_id = r.entity_id
                                 AND e.field = ? AND e.value = ?))
          ${batchBound ? "AND (SELECT batch FROM sync_log l WHERE l.version = r.winner_version) <= ?" : ""}
        ORDER BY r.winner_version ASC
        LIMIT ?`;
    const params = (
      limit: number,
      batch?: number,
    ): (string | number | null)[] => [
      cursor,
      cursor,
      device,
      EXISTENCE,
      EXISTENCE,
      DEAD,
      ...(batch === undefined ? [] : [batch]),
      limit,
    ];

    let rows = sql
      .exec(query(false), ...params(limit))
      .toArray() as unknown as CellRow[];
    const pageFull = rows.length === limit;
    if (pageFull) {
      const last = rows[rows.length - 1];
      // SAFETY: the query selects one column, aliased `batch`.
      const batch = sql.exec(
        "SELECT batch FROM sync_log WHERE version = ?",
        Number(last?.winner_version),
      ).one() as { batch: number };
      // The rest of that batch can be up to a full push, so the extension is
      // allowed to overshoot the page size by one push's worth.
      rows = sql
        .exec(
          query(true),
          ...params(limit + MAX_PUSH, Number(batch.batch)),
        )
        .toArray() as unknown as CellRow[];
    }
    // SAFETY: the query selects one column, aliased `version`.
    const head = sql
      .exec(
        "SELECT COALESCE(MAX(winner_version), ?) AS version FROM sync_register WHERE 1 = 1",
        cursor,
      )
      .one() as { version: number };
    const last = rows[rows.length - 1];
    const delivered = last === undefined ? cursor : Number(last.winner_version);
    // SAFETY: the query selects one column, aliased `more`.
    const beyond = sql
      .exec(
        "SELECT EXISTS(SELECT 1 FROM sync_register WHERE winner_version > ?) AS more",
        delivered,
      )
      .one() as { more: number };
    const hasMore = pageFull && Number(beyond.more) === 1;
    const next = hasMore ? delivered : Math.max(Number(head.version), cursor);
    return { rows: rows.map(toLogRow), cursor: Math.max(next, cursor), hasMore };
  }

  /**
   * Notices for the cells this device lost to other devices, and the pruning
   * that makes them at-least-once: a notice is only dropped once the device's
   * cursor has passed the winner, which cannot happen unless the response that
   * carried both was applied.
   */
  private notices(sql: Sql, cursor: number, device: string): SupersededNotice[] {
    sql.exec(
      "DELETE FROM sync_superseded WHERE device_id = ? AND winner_version <= ?",
      device,
      cursor,
    );
    const rows = sql
      .exec(
        `SELECT event_id, entity_id, field, lost, winner FROM sync_superseded
          WHERE device_id = ? AND winner_version > ?
          ORDER BY winner_version ASC`,
        device,
        cursor,
      )
      .toArray() as unknown as {
      event_id: string;
      entity_id: string;
      field: string;
      lost: string | null;
      winner: string;
    }[];
    return rows.map((row) => ({
      event_id: row.event_id,
      entity_id: row.entity_id,
      field: row.field,
      lost: decode(row.lost),
      winner: JSON.parse(row.winner) as LogRow,
    }));
  }

  /** A pushed cell's outcome, read back from the register. */
  private settle(
    sql: Sql,
    event: { entity_id: string; field: string; event_id: string },
  ): Verdict {
    const winner = this.cell(sql, event.entity_id, event.field);
    if (winner === undefined) {
      return { event_id: event.event_id, outcome: "superseded", winner: null };
    }
    if (winner.event_id === event.event_id) {
      return { event_id: event.event_id, outcome: "accepted" };
    }
    return { event_id: event.event_id, outcome: "superseded", winner: toLogRow(winner) };
  }

  /** What the client needs to repair a refusal: the cell and, if gone, the tombstone. */
  private context(sql: Sql, event: CellEvent): Pick<Verdict, "winner" | "tombstone"> {
    const existence = this.cell(sql, event.entity_id, EXISTENCE);
    const targeted = this.cell(sql, event.entity_id, event.field);
    return {
      winner: targeted === undefined ? null : toLogRow(targeted),
      tombstone: existence?.value === DEAD ? toLogRow(existence) : null,
    };
  }

  private logged(sql: Sql, eventId: string): boolean {
    return (
      sql.exec("SELECT 1 FROM sync_log WHERE event_id = ?", eventId).toArray().length > 0
    );
  }

  private cell(sql: Sql, entityId: string, field: string): CellRow | undefined {
    const rows = sql
      .exec(
        `SELECT entity_id, field, kind, value, timestamp, device_id, event_id,
                winner_version
           FROM sync_register WHERE entity_id = ? AND field = ?`,
        entityId,
        field,
      )
      .toArray() as unknown as CellRow[];
    return rows[0];
  }

  /** Whether a cell has a non-null winner, i.e. the field is populated. */
  private present(sql: Sql, entityId: string, field: string): boolean {
    const row = this.cell(sql, entityId, field);
    return row !== undefined && row.value !== null;
  }
}

/**
 * The page size: the default unless the caller asks for less. A device on a
 * slow link can pull in smaller pieces; a page is still only ever cut between
 * pushes.
 */
function pageSize(url: URL): number {
  const requested = Number(url.searchParams.get("page") ?? "");
  if (!Number.isInteger(requested) || requested < 1) {
    return PAGE_LIMIT;
  }
  return Math.min(requested, PAGE_LIMIT);
}

function isCellEvent(event: unknown): event is CellEvent {
  if (typeof event !== "object" || event === null) {
    return false;
  }
  const candidate = event as Partial<CellEvent>;
  return (
    typeof candidate.event_id === "string" &&
    candidate.event_id !== "" &&
    typeof candidate.device_id === "string" &&
    typeof candidate.entity_id === "string" &&
    candidate.entity_id !== "" &&
    typeof candidate.kind === "string" &&
    typeof candidate.field === "string"
  );
}

function toLogRow(row: CellRow): LogRow {
  return {
    version: Number(row.winner_version),
    event_id: row.event_id,
    device_id: row.device_id,
    entity_id: row.entity_id,
    kind: row.kind,
    field: row.field,
    value: decode(row.value),
    timestamp: Number(row.timestamp),
  };
}
