// The IPC commands of the Rust side. Oids and tids are ZODB-form strings,
// for example "0x00".

import { invoke } from "@tauri-apps/api/core";

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
}

export const openFolder = (path: string) =>
  invoke<FolderInfo>("open_folder", { path });

export const pickFolder = () => invoke<string | null>("pick_folder");

export const root = () => invoke<LoupeObject>("root");

export const object = (oid: string) => invoke<LoupeObject>("object", { oid });

export const resolve = (path: string) =>
  invoke<LoupeObject>("resolve", { path });

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
