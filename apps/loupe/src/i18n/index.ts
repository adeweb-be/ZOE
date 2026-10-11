// The translation catalog of the window. Every UI string comes from it.
// The catalog has one language now. More catalogs can come later with the
// same keys.

import catalog from "./en.json";

/** The key of a UI string. */
export type MessageKey = keyof typeof catalog;

/** Tell if `key` is a key of the catalog. */
export function isMessageKey(key: string): key is MessageKey {
  return Object.hasOwn(catalog, key);
}

/**
 * Return the UI string `key`. Each `{name}` in the string becomes the
 * parameter `name`.
 */
export function t(key: MessageKey, params: Record<string, unknown> = {}): string {
  return catalog[key].replace(/\{(\w+)\}/g, (match, name: string) =>
    name in params ? String(params[name]) : match,
  );
}
