/**
 * The schema table: kinds, their fields, storage classes, links, cascades and
 * kind-derivation rules. It is a data file (`sync_schema.json`) rather than
 * generated code so the Rust side and the worker read the same source and
 * neither has to run the other's toolchain.
 */

import raw from "../../sync_schema.json";
import type { CellValue } from "./types";

export type FieldClass = "text" | "int" | "real" | "scalar";

export interface FieldSpec {
  class: FieldClass;
  /** Must be present when the entity is created. */
  required?: boolean;
  /** May be written once, at create; a later write is refused. */
  immutable?: boolean;
  /** Names the kind the value must point at. */
  link?: string;
  /** The domain column, when it differs from the field name. */
  column?: string;
}

export interface CascadeEdge {
  kind: string;
  field?: string;
  action: "set_null" | "delete";
}

export interface SubkindRule {
  default: string;
  from: [field: string, subkind: string][];
}

export interface KindSpec {
  table: string;
  fields: Record<string, FieldSpec>;
  cascade?: CascadeEdge[];
  subkind?: SubkindRule;
}

interface SchemaFile {
  kinds: Record<string, KindSpec>;
}

export const SCHEMA = raw as unknown as SchemaFile;

export function kindSpec(kind: string): KindSpec | undefined {
  return Object.hasOwn(SCHEMA.kinds, kind) ? SCHEMA.kinds[kind] : undefined;
}

export function fieldSpec(kind: string, field: string): FieldSpec | undefined {
  return kindSpec(kind)?.fields[field];
}

/** Store a cell value as JSON text (`null` stays SQL NULL). */
export function encode(value: CellValue | null): string | null {
  return value === null ? null : JSON.stringify(value);
}

export function decode(text: string | null): CellValue | null {
  return text === null ? null : (JSON.parse(text) as CellValue);
}

/** Whether a JSON body carries exactly one well-formed cell value. */
export function isCellValue(value: unknown): value is CellValue {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const keys = Object.keys(value);
  if (keys.length !== 1) {
    return false;
  }
  const candidate = value as Record<string, unknown>;
  if (Object.hasOwn(candidate, "text")) {
    return typeof candidate.text === "string";
  }
  if (Object.hasOwn(candidate, "int")) {
    return Number.isInteger(candidate.int);
  }
  if (Object.hasOwn(candidate, "real")) {
    return typeof candidate.real === "number" && Number.isFinite(candidate.real);
  }
  return false;
}

export function classMatches(cls: FieldClass, value: CellValue): boolean {
  switch (cls) {
    case "scalar":
      // A tracker's storage class depends on its configured kind, which the
      // server has no copy of, so any one scalar is allowed here.
      return true;
    case "text":
      return Object.hasOwn(value, "text");
    case "int":
      return Object.hasOwn(value, "int");
    case "real":
      return Object.hasOwn(value, "real");
  }
}

/** The subkind a set of field cells implies, e.g. a task's kind from its fields. */
export function subkindFrom(
  rule: SubkindRule,
  present: (field: string) => boolean,
): string {
  for (const [field, subkind] of rule.from) {
    if (present(field)) {
      return subkind;
    }
  }
  return rule.default;
}
