// Sizes and times for the space screen: a byte count in binary units, and
// the time of a tid.

import { type RefObject, useLayoutEffect, useRef, useState } from "react";

import { t } from "./i18n";

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/** Write a byte count in binary units: `512 B`, `1.5 KiB`, `12 MiB`. */
export function formatBytes(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // One decimal below 100, and no ".0".
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  const shown = String(Number(value.toFixed(digits)));
  return t("size.value", { value: shown, unit: UNITS[unit] });
}

/** Write a byte count in full, for a tooltip: `1536 bytes`. */
export function exactBytes(bytes: number): string {
  return t("object.sizeBytes", { size: bytes });
}

/** Write a share of a whole as a percentage with one decimal. */
export function share(part: number, whole: number): string {
  return whole === 0 ? "0.0%" : `${((100 * part) / whole).toFixed(1)}%`;
}

/**
 * Return the time of a tid in ZODB form, in milliseconds since the epoch.
 * A tid is a ZODB `TimeStamp`: the high 4 bytes count the minutes since
 * 1900-01-01 in UTC, with 31 days in each month, and the low 4 bytes are
 * the seconds of the minute in units of 60 / 2^32.
 */
export function tidTime(tid: string): number {
  const value = BigInt(tid);
  let minutes = Number(value >> 32n);
  const fraction = Number(value & 0xffffffffn);
  const minute = minutes % 60;
  minutes = Math.floor(minutes / 60);
  const hour = minutes % 24;
  minutes = Math.floor(minutes / 24);
  const day = (minutes % 31) + 1;
  minutes = Math.floor(minutes / 31);
  const month = minutes % 12;
  const year = Math.floor(minutes / 12) + 1900;
  const seconds = (fraction * 60) / 2 ** 32;
  return Date.UTC(year, month, day, hour, minute) + seconds * 1000;
}

/** Write a time as the date and the time in UTC: `2001-09-09 01:46:47`. */
export function formatTime(time: number): string {
  return new Date(time).toISOString().slice(0, 19).replace("T", " ");
}

/**
 * Return a ref for an element and its width in pixels. The width follows
 * the element as it resizes. Before the first measure, it is `fallback`.
 */
export function useWidth<T extends HTMLElement>(
  fallback: number,
): [RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(fallback);
  useLayoutEffect(() => {
    const element = ref.current;
    if (element === null) {
      return;
    }
    const measure = () => setWidth(element.clientWidth || fallback);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [fallback]);
  return [ref, width];
}
