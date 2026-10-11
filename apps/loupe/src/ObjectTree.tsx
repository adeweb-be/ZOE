// The object tree: the root, and below each object its outbound
// references. A row loads its object when it opens. The rows are
// virtualised, so a tree with many rows stays fast.

import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";

import { type LoupeObject, object, referenceClass } from "./api";
import { t } from "./i18n";

/** One visible row. */
interface Row {
  /** The place of the row: the oids and pointers from the root. */
  key: string;
  oid: string;
  label: string;
  className: string | null;
  depth: number;
}

const ROW_HEIGHT = 26;

interface Props {
  root: LoupeObject;
  selected: string | null;
  onSelect: (oid: string) => void;
  onError: (failure: unknown) => void;
}

export function ObjectTree({ root, selected, onSelect, onError }: Props) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [loaded, setLoaded] = useState<Map<string, LoupeObject>>(
    () => new Map([[root.oid, root]]),
  );
  const scroller = useRef<HTMLDivElement>(null);

  const rows = useMemo(() => {
    const rows: Row[] = [];
    const visit = (row: Row) => {
      rows.push(row);
      const loadedObject = loaded.get(row.oid);
      if (!expanded.has(row.key) || loadedObject === undefined) {
        return;
      }
      for (const reference of loadedObject.references) {
        visit({
          key: `${row.key}/${reference.oid}@${reference.at}`,
          oid: reference.oid,
          label: reference.at,
          className: referenceClass(loadedObject.state, reference.at),
          depth: row.depth + 1,
        });
      }
    };
    visit({
      key: root.oid,
      oid: root.oid,
      label: t("tree.root"),
      className: root.class,
      depth: 0,
    });
    return rows;
  }, [root, expanded, loaded]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });

  // The updates start from the current sets, so that a row that opens
  // while another row loads keeps the other row open.
  const toggle = async (row: Row) => {
    if (expanded.has(row.key)) {
      setExpanded((current) => {
        const next = new Set(current);
        next.delete(row.key);
        return next;
      });
      return;
    }
    if (!loaded.has(row.oid)) {
      try {
        const found = await object(row.oid);
        setLoaded((current) => new Map(current).set(row.oid, found));
      } catch (failure) {
        onError(failure);
        return;
      }
    }
    setExpanded((current) => new Set(current).add(row.key));
  };

  return (
    <div
      className="tree"
      ref={scroller}
      role="tree"
      aria-label={t("tree.label")}
    >
      <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          const open = expanded.has(row.key);
          return (
            <div
              key={row.key}
              className={row.oid === selected ? "row selected" : "row"}
              role="treeitem"
              aria-level={row.depth + 1}
              aria-expanded={open}
              aria-selected={row.oid === selected}
              style={{
                height: item.size,
                transform: `translateY(${item.start}px)`,
                paddingLeft: `${row.depth * 16 + 4}px`,
              }}
            >
              <button
                type="button"
                className="toggle"
                aria-label={open ? t("tree.collapse") : t("tree.expand")}
                onClick={() => void toggle(row)}
              >
                {open ? "-" : "+"}
              </button>
              <button
                type="button"
                className="label"
                onClick={() => onSelect(row.oid)}
              >
                <span className="at">{row.label}</span>
                <span className="class">{row.className ?? ""}</span>
                <span className="oid">{row.oid}</span>
              </button>
            </div>
          );
        })}
      </div>
    </div>
  );
}
