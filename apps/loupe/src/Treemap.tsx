// The treemap of a path: one rectangle for each item, sized by the bytes
// that the item owns, one for the object itself and one for the items past
// the limit. d3 computes the layout and React draws it. A click on an item
// opens the treemap of its path.

import { hierarchy, treemap, treemapSquarify } from "d3";

import type { PathSpace } from "./api";
import { t } from "./i18n";
import { exactBytes, formatBytes, useWidth } from "./size";

/** The height of the treemap in pixels. */
const HEIGHT = 320;

/** The smallest cell that shows its name, in pixels. */
const LABEL_WIDTH = 48;
const LABEL_HEIGHT = 30;

/** One cell of the treemap. */
interface Cell {
  /** The kind of the cell: an item, the object itself, or the others. */
  kind: "item" | "own" | "others";
  label: string;
  /** The text of the tooltip. */
  detail: string;
  bytes: number;
  /** The name of the item, for an item. */
  name?: string;
}

/** The root of the hierarchy: the cells are its children. */
interface Root {
  cells: Cell[];
}

interface Props {
  space: PathSpace;
  /** Open the treemap of the item `name`. */
  onOpen: (name: string) => void;
}

/** Return the cells of a path: its items, itself and the others. */
function cells(space: PathSpace): Cell[] {
  const found: Cell[] = space.items.map((item) => ({
    kind: "item",
    label: item.name,
    detail: t("space.cellItem", {
      name: item.name,
      class: item.class ?? t("object.noClass"),
      oid: item.oid,
      size: exactBytes(item.reach.bytes),
      count: item.reach.count,
    }),
    bytes: item.reach.bytes,
    name: item.name,
  }));
  found.push({
    kind: "own",
    label: t("space.own"),
    detail: t("space.cellOwn", {
      size: exactBytes(space.own.bytes),
      count: space.own.count,
    }),
    bytes: space.own.bytes,
  });
  if (space.others.count > 0) {
    found.push({
      kind: "others",
      label: t("space.others", { count: space.others.count }),
      detail: t("space.cellOthers", {
        items: space.others.count,
        size: exactBytes(space.others.reach.bytes),
        count: space.others.reach.count,
      }),
      bytes: space.others.reach.bytes,
    });
  }
  return found.filter((cell) => cell.bytes > 0);
}

export function Treemap({ space, onOpen }: Props) {
  const [box, width] = useWidth<HTMLDivElement>(640);
  const root = hierarchy<Root | Cell>({ cells: cells(space) }, (node) =>
    "cells" in node ? node.cells : undefined,
  )
    .sum((node) => ("bytes" in node ? node.bytes : 0))
    .sort((a, b) => (b.value ?? 0) - (a.value ?? 0));
  const layout = treemap<Root | Cell>()
    .tile(treemapSquarify)
    .size([width, HEIGHT])
    .paddingInner(2)
    .round(true)(root);
  const leaves = layout.leaves().filter((leaf) => leaf.depth === 1);

  return (
    <div className="treemap" ref={box}>
      <svg
        data-testid="space-treemap"
        width={width}
        height={HEIGHT}
        role="img"
        aria-label={t("space.treemapLabel", { path: space.path })}
      >
        {leaves.map((leaf) => {
          const cell = leaf.data as Cell;
          const w = leaf.x1 - leaf.x0;
          const h = leaf.y1 - leaf.y0;
          const open =
            cell.kind === "item" && cell.name !== undefined
              ? () => onOpen(cell.name as string)
              : undefined;
          return (
            <g
              key={`${cell.kind}:${cell.label}`}
              className={`cell ${cell.kind}`}
              role={open ? "button" : undefined}
              tabIndex={open ? 0 : undefined}
              aria-label={cell.detail}
              onClick={open}
              onKeyDown={(event) => {
                if (open && (event.key === "Enter" || event.key === " ")) {
                  event.preventDefault();
                  open();
                }
              }}
            >
              <title>{cell.detail}</title>
              {/* A nested svg clips the label to the cell. */}
              <svg x={leaf.x0} y={leaf.y0} width={w} height={h}>
                <rect width={w} height={h} rx={4} />
                {w >= LABEL_WIDTH && h >= LABEL_HEIGHT && (
                  <text x={6} y={16}>
                    <tspan className="name">{cell.label}</tspan>
                    <tspan x={6} dy={14} className="size">
                      {formatBytes(cell.bytes)}
                    </tspan>
                  </text>
                )}
              </svg>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
