// Write the JSON state of an object as indented text. Values print as
// JSON, except some tags of the Rust side: bytes print as a bytes literal,
// so their escapes show once; a Python 2 str prints as a string with a
// note; a float prints as its text; a cut value shows its size. The text
// stops at a limit, so a large state does not block the window.

import { t } from "./i18n";

const INDENT = "  ";

/** The text of a state, and if it is complete. */
export interface Formatted {
  text: string;
  complete: boolean;
}

/** Return the size note of a cut value, or nothing. */
function cut(value: Record<string, unknown>): string {
  return typeof value.length === "number"
    ? ` ${t("state.cut", { length: value.length })}`
    : "";
}

/** Return the short form of a tagged value, or null for other values. */
function tagged(value: Record<string, unknown>): string | null {
  if (typeof value.$bytes === "string") {
    return `b'${value.$bytes.replace(/'/g, "\\'")}'${cut(value)}`;
  }
  if (typeof value.$str === "string") {
    return `${JSON.stringify(value.$str)}${cut(value)}`;
  }
  if (typeof value.$str2 === "string") {
    return `${JSON.stringify(value.$str2)} ${t("state.str2")}${cut(value)}`;
  }
  if (typeof value.$float === "string") {
    return value.$float;
  }
  return null;
}

/** Write `state` as indented text of at most about `limit` characters. */
export function formatState(state: unknown, limit: number): Formatted {
  const chunks: string[] = [];
  let size = 0;
  let stopped = false;
  // Refuse a chunk once the text has reached the limit.
  const write = (chunk: string): boolean => {
    if (size >= limit) {
      stopped = true;
      return false;
    }
    chunks.push(chunk);
    size += chunk.length;
    return true;
  };
  // The state has a bounded depth, so the recursion stays shallow.
  const visit = (value: unknown, indent: string): boolean => {
    if (value === null || typeof value !== "object") {
      return write(JSON.stringify(value));
    }
    const inner = indent + INDENT;
    if (Array.isArray(value)) {
      if (value.length === 0) {
        return write("[]");
      }
      if (!write("[\n")) {
        return false;
      }
      for (const [index, item] of value.entries()) {
        const end = index < value.length - 1 ? ",\n" : "\n";
        if (!write(inner) || !visit(item, inner) || !write(end)) {
          return false;
        }
      }
      return write(`${indent}]`);
    }
    const record = value as Record<string, unknown>;
    const short = tagged(record);
    if (short !== null) {
      return write(short);
    }
    const entries = Object.entries(record);
    if (entries.length === 0) {
      return write("{}");
    }
    if (!write("{\n")) {
      return false;
    }
    for (const [index, [key, item]] of entries.entries()) {
      const end = index < entries.length - 1 ? ",\n" : "\n";
      if (
        !write(`${inner}${JSON.stringify(key)}: `) ||
        !visit(item, inner) ||
        !write(end)
      ) {
        return false;
      }
    }
    return write(`${indent}}`);
  };
  visit(state, "");
  return { text: chunks.join(""), complete: !stopped };
}
