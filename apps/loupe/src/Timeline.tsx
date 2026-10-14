// The timeline of the growth: the bytes that the transactions wrote, added
// up over time. The x axis is the time of each tid. d3 computes the scales
// and the path, and React draws them. A crosshair shows the transaction
// nearest to the pointer.

import { area, bisector, line, max, scaleLinear, scaleUtc } from "d3";
import { type MouseEvent, useState } from "react";

import type { Growth } from "./api";
import { t } from "./i18n";
import { exactBytes, formatBytes, formatTime, tidTime, useWidth } from "./size";

const HEIGHT = 220;
const MARGIN = { top: 12, right: 16, bottom: 28, left: 72 };

/** The most points that the path draws. Past it, the path skips points. */
const MAX_POINTS = 2_000;

/** One point of the timeline: a transaction and the bytes so far. */
interface Point {
  tid: string;
  time: number;
  bytes: number;
  total: number;
}

/** Return the points of `growth`, with the bytes added up. */
function points(growth: Growth[]): Point[] {
  let total = 0;
  return growth.map(({ tid, bytes }) => {
    total += bytes;
    return { tid, time: tidTime(tid), bytes, total };
  });
}

/** Return at most `MAX_POINTS` points, evenly spread, with the last one. */
function thin(all: Point[]): Point[] {
  if (all.length <= MAX_POINTS) {
    return all;
  }
  const step = all.length / MAX_POINTS;
  const kept: Point[] = [];
  for (let at = 0; at < all.length; at += step) {
    kept.push(all[Math.floor(at)]);
  }
  kept.push(all[all.length - 1]);
  return kept;
}

const byTime = bisector((point: Point) => point.time);

export function Timeline({ growth }: { growth: Growth[] }) {
  const [box, width] = useWidth<HTMLDivElement>(640);
  const [hover, setHover] = useState<Point | null>(null);
  const all = points(growth);
  const shown = thin(all);
  const first = all[0]?.time ?? 0;
  const last = all[all.length - 1]?.time ?? 0;
  // One transaction, or many at one time, still gives a span.
  const pad = first === last ? 60_000 : 0;
  const x = scaleUtc()
    .domain([first - pad, last + pad])
    .range([MARGIN.left, Math.max(MARGIN.left + 1, width - MARGIN.right)]);
  // The ticks fall on round numbers of the binary unit of the top.
  const top = max(all, (point) => point.total) || 1;
  const unit = 1024 ** Math.floor(Math.log(Math.max(top, 1)) / Math.log(1024));
  const units = scaleLinear().domain([0, top / unit]).nice();
  const ticks = units.ticks(5).map((tick) => tick * unit);
  const y = scaleLinear()
    .domain([0, units.domain()[1] * unit])
    .range([HEIGHT - MARGIN.bottom, MARGIN.top]);
  const path = line<Point>()
    .x((point) => x(point.time))
    .y((point) => y(point.total))(shown);
  const fill = area<Point>()
    .x((point) => x(point.time))
    .y0(y(0))
    .y1((point) => y(point.total))(shown);

  /** Show the transaction nearest to the pointer. */
  const move = (event: MouseEvent<SVGSVGElement>) => {
    if (all.length === 0) {
      return;
    }
    const bounds = event.currentTarget.getBoundingClientRect();
    const time = x.invert(event.clientX - bounds.left).getTime();
    const at = Math.min(byTime.center(all, time), all.length - 1);
    setHover(all[at]);
  };

  return (
    <div className="timeline" ref={box}>
      <svg
        data-testid="space-growth"
        width={width}
        height={HEIGHT}
        role="img"
        aria-label={t("space.growthLabel", { count: growth.length })}
        onMouseMove={move}
        onMouseLeave={() => setHover(null)}
      >
        <g className="grid">
          {ticks.map((tick) => (
            <g key={tick} transform={`translate(0,${y(tick)})`}>
              <line x1={MARGIN.left} x2={width - MARGIN.right} />
              <text x={MARGIN.left - 6} dy="0.32em" textAnchor="end">
                {formatBytes(tick)}
              </text>
            </g>
          ))}
        </g>
        <g className="axis">
          {x.ticks(Math.max(2, Math.floor(width / 140))).map((tick) => (
            <text
              key={tick.getTime()}
              x={x(tick)}
              y={HEIGHT - MARGIN.bottom + 18}
              textAnchor="middle"
            >
              {x.tickFormat()(tick)}
            </text>
          ))}
        </g>
        {fill && <path className="area" d={fill} />}
        {path && <path className="line" d={path} />}
        {hover && (
          <g className="crosshair">
            <line
              x1={x(hover.time)}
              x2={x(hover.time)}
              y1={MARGIN.top}
              y2={HEIGHT - MARGIN.bottom}
            />
            <circle cx={x(hover.time)} cy={y(hover.total)} r={4} />
          </g>
        )}
      </svg>
      {hover && (
        <p className="tooltip" data-testid="space-growth-point">
          {t("space.growthPoint", {
            tid: hover.tid,
            time: formatTime(hover.time),
            bytes: exactBytes(hover.bytes),
            total: exactBytes(hover.total),
          })}
        </p>
      )}
    </div>
  );
}
