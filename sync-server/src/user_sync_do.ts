/**
 * The per-user Durable Object: an append-only cell log plus a materialised
 * register that names the winning event of every `(entity_id, field)` cell.
 *
 * A push is validated cell by cell inside one transaction — nothing is ever
 * appended and then retracted — and every pushed event gets a verdict. A pull
 * is a register diff: cells whose winner sits above the client's cursor, with
 * the client's own cells suppressed unless it is materialising from nothing.
 *
 * A round that cannot mean anything is refused before anything is appended: a
 * cursor minted by a different log, one past the end of this log, or a peer that
 * speaks another protocol or cell vocabulary. Each of those would otherwise be
 * read as a position in this log and quietly lose the user's cells.
 *
 * The register is a pure fold of the log (see `fold`), so it is a cache that
 * can be rebuilt, never a second truth.
 */

import {
  PROTOCOL,
  SCHEMA_VERSION,
  classMatches,
  decode,
  encode,
  fieldSpec,
  isCellValue,
  isEncryptedValue,
  kindSpec,
  subkindFrom,
} from "./schema";
import { EXISTENCE, error, isCellCursor, json, refusal } from "./types";
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

/** A push that cannot land whole, so none of it lands. */
class Refused extends Error {}

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
  /** The log identity, read once per object instance. */
  private minted: string | undefined;

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
    // Facts about this log that outlive a restart. Its identity is what a client
    // stores beside its cursor, so "my position belongs here" can be told from
    // "my position belongs to another log".
    sql.exec(
      `CREATE TABLE IF NOT EXISTS sync_meta (
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
       )`,
    );
  }

  private keyId(sql: Sql): string | undefined {
    const rows = sql.exec("SELECT value FROM sync_meta WHERE key = 'key_id'").toArray() as {
      value: string;
    }[];
    return rows[0]?.value;
  }

  /**
   * The identity of this log: minted with the log and kept for as long as that
   * log exists, so a cursor issued here stays recognisable across restarts.
   */
  private logId(sql: Sql): string {
    if (this.minted !== undefined) {
      return this.minted;
    }
    sql.exec(
      "INSERT OR IGNORE INTO sync_meta (key, value) VALUES ('log_id', ?)",
      crypto.randomUUID(),
    );
    // SAFETY: the query selects one column, aliased `value`.
    const row = sql.exec("SELECT value FROM sync_meta WHERE key = 'log_id'").one() as {
      value: string;
    };
    this.minted = row.value;
    return this.minted;
  }

  /** This log's identity and the last version it issued. */
  private identity(): { log: string; head: number; storedKeyId: string | undefined } {
    return this.ctx.storage.transactionSync(() => {
      const sql = this.ctx.storage.sql;
      // SAFETY: the query selects one column, aliased `head`.
      const head = Number(
        (
          sql.exec("SELECT COALESCE(MAX(version), 0) AS head FROM sync_log").one() as {
            head: number;
          }
        ).head,
      );
      return { log: this.logId(sql), head, storedKeyId: this.keyId(sql) };
    });
  }

  private async sync(request: Request, pageSize: number): Promise<Response> {
    let body: SyncRequest;
    try {
      body = (await request.json()) as SyncRequest;
    } catch {
      return error("invalid json body", 400);
    }
    // The vocabulary first: a peer that speaks another one cannot be read, and
    // the alternative — skipping the cells it cannot parse and advancing its
    // cursor past them — loses data silently.
    if (body?.protocol !== PROTOCOL || body.schema !== SCHEMA_VERSION) {
      return refusal(
        "version_mismatch",
        `this server speaks sync protocol ${PROTOCOL} and schema ${SCHEMA_VERSION}`,
        { protocol: PROTOCOL, schema: SCHEMA_VERSION },
      );
    }
    if (typeof body.device_id !== "string" || body.device_id === "") {
      return error("device_id is required", 400);
    }
    if (body.key_id !== undefined && (typeof body.key_id !== "string" || body.key_id === "")) {
      return error("key_id must be a non-empty string", 400);
    }
    if (!Array.isArray(body.events)) {
      return error("events must be an array", 400);
    }
    if (body.events.length > MAX_PUSH) {
      return error(`at most ${MAX_PUSH} events per push`, 400);
    }
    const cursor = body.cursor ?? null;
    if (cursor !== null && !isCellCursor(cursor)) {
      return error("cursor must be { log: string, version: number }, or null", 400);
    }
    const device = body.device_id;
    const now = Date.now();

    const { log, head, storedKeyId } = this.identity();
    if (storedKeyId !== undefined && storedKeyId !== body.key_id) {
      return refusal("key_mismatch", "the encryption key does not match this log", {
        log_id: log,
        head,
        key_id: storedKeyId,
      });
    }
    // A cursor from another log would read the wrong history through this one,
    // and one past this log's end would read nothing at all — both silently,
    // and for good. A database that has read nowhere yet names no log, so it
    // adopts this one.
    if (cursor !== null && cursor.log !== log) {
      return refusal("log_mismatch", "the cursor belongs to another log", { log_id: log, head });
    }
    if (cursor !== null && cursor.version > head) {
      return refusal("cursor_ahead", "the cursor is past the end of this log", {
        log_id: log,
        head,
      });
    }

    try {
      return json(this.pushBatch(body, device, log, cursor?.version ?? 0, now, pageSize));
    } catch (failure) {
      if (failure instanceof Refused) {
        return error(failure.message, 400);
      }
      throw failure;
    }
  }

  /**
   * Validate and append the batch, then serve the pull that follows it.
   *
   * Throws `Refused` when the push cannot land whole: the transaction rolls
   * back, so a refusal leaves no trace.
   */
  private pushBatch(
    body: SyncRequest,
    device: string,
    log: string,
    cursor: number,
    now: number,
    pageSize: number,
  ): SyncResponse {
    return this.ctx.storage.transactionSync(() => {
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
      const requireEncrypted = body.key_id !== undefined || this.keyId(sql) !== undefined;
      const verdicts: (Verdict | null)[] = body.events.map(() => null);
      const pushed: { index: number; entity_id: string; field: string; event_id: string }[] = [];
      const creating = this.validateCreates(sql, body.events);
      const rejectedCreates = this.prevalidateCreates(
        sql,
        body.events,
        creating,
        device,
        now,
        requireEncrypted,
      );

      body.events.forEach((raw, index) => {
        const candidate = raw as Partial<CellEvent> | null;
        const entityId = typeof candidate?.entity_id === "string" ? candidate.entity_id : "";
        const rejectedCreate = rejectedCreates.get(entityId);
        if (rejectedCreate !== undefined) {
          const eventId = typeof candidate?.event_id === "string" ? candidate.event_id : "";
          verdicts[index] = { ...rejectedCreate, event_id: eventId };
          return;
        }
        const result = this.push(sql, raw, device, now, creating, batch, requireEncrypted);
        if (result.verdict.outcome === "rejected") {
          verdicts[index] = result.verdict;
        } else {
          pushed.push({ index, ...result.cell });
        }
      });
      if (body.key_id !== undefined && pushed.length > 0) {
        sql.exec(
          "INSERT OR IGNORE INTO sync_meta (key, value) VALUES ('key_id', ?)",
          body.key_id,
        );
      }
      // A cell is only accepted or superseded once the whole batch has landed:
      // a later event of the same push can legitimately take the cell away
      // from an earlier one.
      for (const event of pushed) {
        verdicts[event.index] = this.settle(sql, event);
      }

      const pull = this.pull(sql, cursor, device, pageSize);
      return {
        cursor: { log, version: pull.cursor },
        now,
        has_more: pull.hasMore,
        events: pull.rows,
        verdicts: verdicts.filter((verdict): verdict is Verdict => verdict !== null),
        superseded: this.notices(sql, cursor, device),
      };
    });
  }

  /**
   * Entities created in this push. Every create must carry all required fields
   * of its kind in the same push so no peer ever sees a half-born row.
   */
  private validateCreates(sql: Sql, events: unknown[]): Set<string> {
    const creating = new Set<string>();
    const populated = new Map<string, Set<string>>();
    const kinds = new Map<string, string>();

    for (const raw of events) {
      if (!isCellEvent(raw)) {
        continue;
      }
      if (
        raw.field === EXISTENCE &&
        raw.value !== null &&
        "text" in raw.value &&
        raw.value.text === "live" &&
        this.cell(sql, raw.entity_id, EXISTENCE) === undefined
      ) {
        creating.add(raw.entity_id);
        kinds.set(raw.entity_id, raw.kind);
      } else if (raw.field !== EXISTENCE && raw.value !== null) {
        let fields = populated.get(raw.entity_id);
        if (fields === undefined) {
          fields = new Set<string>();
          populated.set(raw.entity_id, fields);
        }
        fields.add(raw.field);
      }
    }

    for (const entity of creating) {
      const kind = kinds.get(entity) ?? "";
      const spec = kindSpec(kind);
      if (spec === undefined) {
        continue;
      }
      const present = populated.get(entity);
      for (const [name, field] of Object.entries(spec.fields)) {
        if (field.required === true && !present?.has(name)) {
          throw new Refused(
            `create of ${entity} (${kind}) is missing required field "${name}": one push must carry a whole create`,
          );
        }
      }
    }
    return creating;
  }

  /**
   * Pre-validate every create burst before appending any cell. If the existence
   * cell, any required field, or any cell's clock stamp fails validation (or a
   * required link points to another create in the batch that failed), the whole
   * create is rejected up front and never touches the log or register. Optional
   * field failures (such as an optional link to a deleted entity) are rejected
   * individually during `push()` without dropping the entity create itself.
   */
  private prevalidateCreates(
    sql: Sql,
    events: unknown[],
    creating: Set<string>,
    device: string,
    now: number,
    requireEncrypted = false,
  ): Map<string, Verdict> {
    const acceptedCreates = new Map<string, string>();
    for (const raw of events) {
      if (isCellEvent(raw) && raw.field === EXISTENCE && creating.has(raw.entity_id)) {
        acceptedCreates.set(raw.entity_id, raw.kind);
      }
    }
    const rejectedCreates = new Map<string, Verdict>();
    let changed = true;
    while (changed) {
      changed = false;
      for (const raw of events) {
        const candidate = raw as Partial<CellEvent> | null;
        const entityId = typeof candidate?.entity_id === "string" ? candidate.entity_id : "";
        if (!acceptedCreates.has(entityId)) {
          continue;
        }
        const fallbackId = typeof candidate?.event_id === "string" ? candidate.event_id : "";
        if (!isCellEvent(candidate)) {
          acceptedCreates.delete(entityId);
          creating.delete(entityId);
          rejectedCreates.set(entityId, {
            event_id: fallbackId,
            outcome: "rejected",
            reason: "shape",
          });
          changed = true;
          continue;
        }
        if (this.logged(sql, candidate.event_id)) {
          continue;
        }
        const field = fieldSpec(candidate.kind, candidate.field);
        if (
          candidate.field !== EXISTENCE &&
          field?.required !== true &&
          candidate.timestamp <= now + SKEW_MS
        ) {
          continue;
        }
        const rejection = this.validateCell(
          sql,
          candidate,
          device,
          now,
          creating,
          acceptedCreates,
          requireEncrypted,
        );
        if (rejection !== null) {
          acceptedCreates.delete(entityId);
          creating.delete(entityId);
          rejectedCreates.set(entityId, rejection);
          changed = true;
        }
      }
    }
    return rejectedCreates;
  }

  /**
   * Validate one cell event against the schema, clock, and register state.
   * Returns a rejection verdict on failure, or `null` when the cell is valid.
   */
  private validateCell(
    sql: Sql,
    event: CellEvent,
    device: string,
    now: number,
    creating: Set<string>,
    acceptedCreates?: Map<string, string>,
    requireEncrypted = false,
  ): Verdict | null {
    const reject = (reason: RejectReason, withCells = true): Verdict => ({
      event_id: event.event_id,
      outcome: "rejected",
      reason,
      ...(withCells ? this.context(sql, event) : {}),
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
      return null;
    }

    if (existence === undefined && !acceptedCreates?.has(event.entity_id)) {
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
    } else if (requireEncrypted && field.link === undefined && !isEncryptedValue(event.value)) {
      return reject("class", false);
    }
    if (field.immutable === true && this.cell(sql, event.entity_id, event.field) !== undefined) {
      return reject("immutable");
    }
    if (field.link !== undefined && event.value !== null) {
      if (isEncryptedValue(event.value)) {
        return reject("class", false);
      }
      const referent = "text" in event.value ? event.value.text : null;
      if (referent === null) {
        return reject("class", false);
      }
      if (event.field === "parent_id" && referent === event.entity_id) {
        return reject("self_parent", false);
      }
      const target = this.cell(sql, referent, EXISTENCE);
      const inBatchKind = acceptedCreates?.get(referent);
      if ((target === undefined || target.kind !== field.link) && inBatchKind !== field.link) {
        return reject("dangling");
      }
      if (target?.value === DEAD) {
        return reject("target_deleted");
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
        return reject("kind_change");
      }
    }
    return null;
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
    requireEncrypted = false,
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
    if (this.logged(sql, event.event_id)) {
      return {
        verdict: { event_id: event.event_id, outcome: "accepted" },
        cell: { entity_id: event.entity_id, field: event.field, event_id: event.event_id },
      };
    }
    const rejection = this.validateCell(sql, event, device, now, creating, undefined, requireEncrypted);
    if (rejection !== null) {
      return {
        verdict: rejection,
        cell: { entity_id: event.entity_id, field: event.field, event_id: event.event_id },
      };
    }

    if (event.field === EXISTENCE) {
      const marker = event.value !== null && "text" in event.value ? event.value.text : null;
      const existence = this.cell(sql, event.entity_id, EXISTENCE);
      // A repeat `live` (deterministic ID collision) or repeat `deleted` is an
      // idempotent no-op: keep the original existence cell untouched.
      if (
        (marker === "live" && existence?.value === LIVE) ||
        (marker === "deleted" && existence?.value === DEAD)
      ) {
        return {
          verdict: { event_id: event.event_id, outcome: "accepted" },
          cell: { entity_id: event.entity_id, field: EXISTENCE, event_id: event.event_id },
        };
      }
      const appended = this.append(sql, event, now, batch);
      if (marker === "deleted" && appended.won) {
        this.cascade(sql, event.kind, event.entity_id, device, event.timestamp, now, batch);
      }
      return {
        verdict: { event_id: event.event_id, outcome: "accepted" },
        cell: { entity_id: event.entity_id, field: EXISTENCE, event_id: event.event_id },
      };
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
    if (
      !quiet &&
      won &&
      event.field !== EXISTENCE &&
      before !== undefined &&
      before.device_id !== event.device_id
    ) {
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
   *
   * Existence cells move one way (`absent -> live -> deleted`): a second `live`
   * never advances the birth version past the entity's fields, and `deleted`
   * always wins over `live` regardless of clock skew between the two devices.
   */
  private fold(sql: Sql, event: CellEvent, version: number, value: string | null): void {
    if (event.field === EXISTENCE) {
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
         WHERE excluded.value = ? AND sync_register.value != ?`,
        event.entity_id,
        EXISTENCE,
        event.kind,
        value,
        event.timestamp,
        event.device_id,
        event.event_id,
        version,
        DEAD,
        DEAD,
      );
      return;
    }

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
    device: string,
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
      const edgeField = edge.field;
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
            edgeField,
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
              device_id: device,
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
            WHERE kind = ? AND field = ? AND value = ?
              AND entity_id NOT IN (
                SELECT entity_id FROM sync_register
                 WHERE field = ? AND value = ?
              )`,
          edge.kind,
          edgeField,
          link,
          EXISTENCE,
          DEAD,
        )
        .toArray() as unknown as { entity_id: string; timestamp: number }[];
      for (const row of stale) {
        this.append(
          sql,
          {
            event_id: crypto.randomUUID(),
            device_id: device,
            entity_id: row.entity_id,
            kind: edge.kind,
            field: edgeField,
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

  /**
   * A pull is a register diff over `(cursor, head]`.
   *
   * The cursor is the last winner version delivered, which is a safe page key
   * because a cell's `winner_version` only moves forward: a write that loses
   * leaves it untouched, and one that wins takes the new append's version.
   *
   * A page may only break *between* pushes, and every live entity whose birth
   * cell lands in the page brings all of its current register cells with it, so
   * a create is always insertable from the page that introduces it even when
   * one of its required fields was rewritten in a later batch.
   */
  private pull(
    sql: Sql,
    cursor: number,
    device: string,
    limit: number,
  ): { rows: LogRow[]; cursor: number; hasMore: boolean } {
    let rows = this.selectPage(sql, cursor, device, limit);
    const pageFull = rows.length === limit;
    let cutoff: number | undefined;
    if (pageFull) {
      const last = rows[rows.length - 1];
      // Extend to the last version of the batch that delivered the cut, so the
      // page ends where a push ended.
      // SAFETY: the query selects one column, aliased `through`.
      const through = sql
        .exec(
          `SELECT MAX(version) AS through FROM sync_log
            WHERE batch = (SELECT batch FROM sync_log WHERE version = ?)`,
          Number(last?.winner_version),
        )
        .one() as { through: number };
      cutoff = Number(through.through);
      const cutRows = this.selectPage(sql, cursor, device, -1, cutoff);
      const hydratedRows = this.hydrateCreates(sql, cursor, cutoff);
      const seen = new Set(cutRows.map((row) => `${row.entity_id}\0${row.field}`));
      for (const extra of hydratedRows) {
        if (!seen.has(`${extra.entity_id}\0${extra.field}`)) {
          cutRows.push(extra);
        }
      }
      cutRows.sort((a, b) => Number(a.winner_version) - Number(b.winner_version));
      rows = cutRows;
    }
    // SAFETY: the query selects one column, aliased `version`.
    const head = sql
      .exec(
        "SELECT COALESCE(MAX(winner_version), ?) AS version FROM sync_register WHERE 1 = 1",
        cursor,
      )
      .one() as { version: number };
    const last = rows[rows.length - 1];
    const delivered =
      cutoff !== undefined
        ? cutoff
        : last === undefined
          ? cursor
          : Number(last.winner_version);
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
   * Select register cells after `cursor` (and optionally at or below `through`).
   * At `cursor === 0` (fresh/reset pull), own-authored cells are included;
   * above `0`, own echoes are suppressed. Dead entities serve only their
   * existence tombstone.
   */
  private selectPage(
    sql: Sql,
    cursor: number,
    device: string,
    limit: number,
    through?: number,
  ): CellRow[] {
    const bound = through === undefined ? "" : "AND r.winner_version <= ?";
    const boundParams = through === undefined ? [] : [through];
    if (cursor === 0) {
      return sql
        .exec(
          `SELECT r.entity_id, r.field, r.kind, r.value, r.timestamp, r.device_id,
                  r.event_id, r.winner_version
             FROM sync_register r
            WHERE r.winner_version > 0
              AND (r.field = ?
                   OR NOT EXISTS (SELECT 1 FROM sync_register e
                                   WHERE e.entity_id = r.entity_id
                                     AND e.field = ? AND e.value = ?))
              ${bound}
            ORDER BY r.winner_version ASC
            LIMIT ?`,
          EXISTENCE,
          EXISTENCE,
          DEAD,
          ...boundParams,
          limit,
        )
        .toArray() as unknown as CellRow[];
    }
    return sql
      .exec(
        `SELECT r.entity_id, r.field, r.kind, r.value, r.timestamp, r.device_id,
                r.event_id, r.winner_version
           FROM sync_register r
          WHERE r.winner_version > ?
            AND r.device_id != ?
            AND (r.field = ?
                 OR NOT EXISTS (SELECT 1 FROM sync_register e
                                 WHERE e.entity_id = r.entity_id
                                   AND e.field = ? AND e.value = ?))
            ${bound}
          ORDER BY r.winner_version ASC
          LIMIT ?`,
        cursor,
        device,
        EXISTENCE,
        EXISTENCE,
        DEAD,
        ...boundParams,
        limit,
      )
      .toArray() as unknown as CellRow[];
  }

  /**
   * Fetch current register cells whose `winner_version` moved past `through` for
   * any live entity born within `(cursor, through]`, so bounded pages always
   * deliver complete creates.
   */
  private hydrateCreates(sql: Sql, cursor: number, through: number): CellRow[] {
    return sql
      .exec(
        `SELECT r.entity_id, r.field, r.kind, r.value, r.timestamp, r.device_id,
                r.event_id, r.winner_version
           FROM sync_register r
          WHERE r.winner_version > ?
            AND EXISTS (
              SELECT 1 FROM sync_register c
               WHERE c.entity_id = r.entity_id
                 AND c.field = ? AND c.value = ?
                 AND c.winner_version > ? AND c.winner_version <= ?
            )
          ORDER BY r.winner_version ASC`,
        through,
        EXISTENCE,
        LIVE,
        cursor,
        through,
      )
      .toArray() as unknown as CellRow[];
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
    if (event.field === EXISTENCE || winner.event_id === event.event_id) {
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
