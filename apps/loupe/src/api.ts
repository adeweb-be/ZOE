// The IPC commands of the Rust side. Oids and tids are ZODB-form strings,
// for example "0x00".

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { isMessageKey, t } from "./i18n";

/** An outbound reference: the oid and a JSON pointer into the state. */
export interface Reference {
  oid: string;
  at: string;
}

/** An object of the store, as the Rust side gives it. */
export interface LoupeObject {
  oid: string;
  serial: string;
  class: string | null;
  size: number;
  state: unknown;
  references: Reference[];
  error: string | null;
}

/** An open store folder. */
export interface FolderInfo {
  path: string;
  lastTid: string;
  /**
   * True when the open repaired the store: a writer left it open, and
   * Loupe wrote its database file once.
   */
  repaired: boolean;
}

/** One revision of an object: the transaction that wrote a record. */
export interface Revision {
  tid: string;
  user: string;
  description: string;
  /** The size of the record in bytes. A deletion has size 0. */
  size: number;
}

/**
 * One change of a diff: a JSON pointer into the two states, and the value
 * before and after. A side that does not have the value has no field.
 */
export interface Change {
  path: string;
  before?: unknown;
  after?: unknown;
}

/** The most revisions that `history` gives: `HISTORY_LIMIT` of zoe-loupe. */
export const HISTORY_LIMIT = 4096;

/** A number of records or objects and their bytes. */
export interface Totals {
  count: number;
  bytes: number;
}

/** One of the largest current records. */
export interface Large {
  oid: string;
  class: string | null;
  size: number;
}

/** What one transaction wrote. */
export interface Growth {
  tid: string;
  bytes: number;
}

/** The space of the store as of its last transaction. */
export interface Space {
  /** The current records of each class. The key `<unknown>` holds the
   * records whose class pickle gives no name. */
  by_class: Record<string, Totals>;
  /** The current blob files. */
  blobs: Totals;
  /** The largest current records, biggest first. */
  largest: Large[];
  /** What each transaction wrote, in tid order. */
  growth: Growth[];
}

/** One item of a path, with the objects that it owns. */
export interface PathItem {
  name: string;
  oid: string;
  class: string | null;
  reach: Totals;
}

/** The space of a path: what it reaches, shared by its items. */
export interface PathSpace {
  path: string;
  oid: string;
  class: string | null;
  /** Every object that the object reaches, itself included, each once. */
  reach: Totals;
  /** The object, and what it reaches that no item owns. */
  own: Totals;
  /** The largest items, biggest first, at most `ITEM_LIMIT` of zoe-loupe. */
  items: PathItem[];
  /** The items past that limit. */
  others: { count: number; reach: Totals };
}

/** What a pack at `tid` would remove: the dry run of the core. */
export interface Reclaim {
  tid: string;
  records: number;
  bytes: number;
  objects: number;
  blobs: number;
  blob_bytes: number;
  by_class: Record<string, Totals>;
}

/** The progress of a background job of the Rust side. */
export interface JobProgress {
  job: "space" | "pathSpace" | "reclaimable";
  walk: "records" | "transactions" | "objects";
  done: number;
}

/** The number of largest records that the space screen lists. */
export const LARGEST = 10;

// `open_folder` and `set_as_of` move the "as of" switch of the Rust side.
// They go through one queue, one call at a time and in order, so the last
// call wins. A read of an object waits for the calls before it, so it
// reads as of the switch that the window asked for last.
let switchCalls: Promise<unknown> = Promise.resolve();

/** Add a call that moves the switch to the queue. */
function moveSwitch<T>(call: () => Promise<T>): Promise<T> {
  const next = switchCalls.then(call);
  switchCalls = next.catch(() => undefined);
  return next;
}

/** Run a read after the calls in the queue. */
function afterSwitch<T>(call: () => Promise<T>): Promise<T> {
  return switchCalls.then(call);
}

export const openFolder = (path: string) =>
  moveSwitch(() => invoke<FolderInfo>("open_folder", { path }));

export const pickFolder = () => invoke<string | null>("pick_folder");

export const root = () => afterSwitch(() => invoke<LoupeObject>("root"));

export const object = (oid: string) =>
  afterSwitch(() => invoke<LoupeObject>("object", { oid }));

export const resolve = (path: string) =>
  afterSwitch(() => invoke<LoupeObject>("resolve", { path }));

export const history = (oid: string) =>
  invoke<Revision[]>("history", { oid });

/** Set the "as of" switch to a tid, or to null for the last transaction. */
export const setAsOf = (tid: string | null) =>
  moveSwitch(() => invoke<null>("set_as_of", { tid }));

export const diff = (oid: string, tidA: string, tidB: string) =>
  invoke<Change[]>("diff", { oid, tidA, tidB });

// The commands of the space screen read the last transaction, whatever the
// switch holds. Each one is a background job of the Rust side.

export const space = (largest: number) =>
  afterSwitch(() => invoke<Space>("space", { largest }));

export const pathSpace = (path: string) =>
  afterSwitch(() => invoke<PathSpace>("path_space", { path }));

/** Return what a pack at a tid, or at the last transaction, would remove. */
export const reclaimable = (tid: string | null) =>
  afterSwitch(() => invoke<Reclaim>("reclaimable", { tid }));

/**
 * Call `handler` with each progress event of the background jobs. Return
 * a function that stops the calls.
 */
export function onProgress(handler: (progress: JobProgress) => void): () => void {
  const stop = listen<JobProgress>("job-progress", (event) =>
    handler(event.payload),
  );
  return () => {
    void stop.then((unlisten) => unlisten());
  };
}

/** Return the value at the JSON pointer `at` in `value`. */
export function pointer(value: unknown, at: string): unknown {
  let current = value;
  for (const raw of at.split("/").slice(1)) {
    const key = raw.replace(/~1/g, "/").replace(/~0/g, "~");
    if (current === null || typeof current !== "object") {
      return undefined;
    }
    current = (current as Record<string, unknown>)[key];
  }
  return current;
}

/** Return the class that a reference in a state names, if any. */
export function referenceClass(state: unknown, at: string): string | null {
  const target = pointer(state, at);
  if (target !== null && typeof target === "object" && "class" in target) {
    const name = (target as { class: unknown }).class;
    return typeof name === "string" ? name : null;
  }
  return null;
}

/**
 * Return the UI string of an error of an IPC call. The Rust side gives
 * `{ kind, ...parameters }`, and the catalog has the key `error.<kind>`.
 */
export function describe(failure: unknown): string {
  if (failure !== null && typeof failure === "object" && "kind" in failure) {
    const params = failure as Record<string, unknown>;
    const key = `error.${String(params.kind)}`;
    if (isMessageKey(key)) {
      return t(key, params);
    }
  }
  return t("error.other", { message: String(failure) });
}
