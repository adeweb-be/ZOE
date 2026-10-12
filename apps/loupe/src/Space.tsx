// The space screen: what the store takes, as of its last transaction. It
// shows the treemap of a path and its items, the classes with their
// records and bytes, the blobs, the largest records, the timeline of the
// growth, and what a pack would remove. The walks are background jobs of
// the Rust side, and the screen shows their progress.

import { type FormEvent, useEffect, useState } from "react";

import {
  type JobProgress,
  LARGEST,
  onProgress,
  type PathSpace,
  pathSpace,
  type Reclaim,
  reclaimable,
  type Space,
  space,
  type Totals,
} from "./api";
import { type MessageKey, t } from "./i18n";
import { exactBytes, formatBytes, share } from "./size";
import { Timeline } from "./Timeline";
import { Treemap } from "./Treemap";

interface Props {
  /** The tid of the last transaction of the folder. */
  lastTid: string;
  /** Show the object `oid` in the object browser. */
  onShow: (oid: string) => void;
  onError: (failure: unknown) => void;
}

/** The UI string of each walk of a job. */
const WALKS: Record<JobProgress["walk"], MessageKey> = {
  records: "space.walkRecords",
  transactions: "space.walkTransactions",
  objects: "space.walkObjects",
};

/** Return the path of the item `name` of the path `path`. */
function child(path: string, name: string): string {
  return path === "/" ? `/${name}` : `${path}/${name}`;
}

/** Return each step of a path: its name and its path, from the root. */
function steps(path: string): { name: string; path: string }[] {
  const found = [{ name: t("space.root"), path: "/" }];
  let at = "/";
  for (const name of path.split("/").filter((s) => s !== "")) {
    at = child(at, name);
    found.push({ name, path: at });
  }
  return found;
}

/** Return the entries of a map of totals, biggest first. */
function biggest(map: Record<string, Totals>): [string, Totals][] {
  return Object.entries(map).sort(
    ([a, x], [b, y]) => y.bytes - x.bytes || a.localeCompare(b),
  );
}

/** Show a byte count in binary units, with the exact count as a tooltip. */
function Bytes({ bytes }: { bytes: number }) {
  return <span title={exactBytes(bytes)}>{formatBytes(bytes)}</span>;
}

/** Show the progress of a job, or a note that it runs. */
function Running({ progress }: { progress: JobProgress | undefined }) {
  return (
    <p className="note" role="status">
      {progress
        ? t(WALKS[progress.walk], { done: progress.done })
        : t("space.running")}
    </p>
  );
}

export function SpaceScreen({ lastTid, onShow, onError }: Props) {
  const [found, setFound] = useState<Space | null>(null);
  const [spaceFailed, setSpaceFailed] = useState(false);
  const [path, setPath] = useState("/");
  const [pathFound, setPathFound] = useState<PathSpace | null>(null);
  const [pathFailed, setPathFailed] = useState(false);
  const [progress, setProgress] = useState<Partial<Record<JobProgress["job"], JobProgress>>>({});
  const [packTid, setPackTid] = useState("");
  const [reclaim, setReclaim] = useState<Reclaim | null>(null);
  const [reclaiming, setReclaiming] = useState(false);

  useEffect(
    () =>
      onProgress((event) =>
        setProgress((jobs) => ({ ...jobs, [event.job]: event })),
      ),
    [],
  );

  // Each effect drops a reply that comes after the screen goes. The parent
  // gives a new `onError` at each render, so no effect depends on it.
  useEffect(() => {
    let live = true;
    space(LARGEST).then(
      (measured) => live && setFound(measured),
      (failure) => {
        if (live) {
          setSpaceFailed(true);
          onError(failure);
        }
      },
    );
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    let live = true;
    setPathFound(null);
    setPathFailed(false);
    setProgress((jobs) => ({ ...jobs, pathSpace: undefined }));
    pathSpace(path).then(
      (measured) => live && setPathFound(measured),
      (failure) => {
        if (live) {
          setPathFailed(true);
          onError(failure);
        }
      },
    );
    return () => {
      live = false;
    };
  }, [path]);

  const pack = (event: FormEvent) => {
    event.preventDefault();
    const tid = packTid.trim() === "" ? null : packTid.trim();
    setReclaim(null);
    setReclaiming(true);
    reclaimable(tid).then(
      (report) => {
        setReclaim(report);
        setReclaiming(false);
      },
      (failure) => {
        setReclaiming(false);
        onError(failure);
      },
    );
  };

  const classes = found ? biggest(found.by_class) : [];
  const total = classes.reduce(
    (sum, [, totals]) => ({
      count: sum.count + totals.count,
      bytes: sum.bytes + totals.bytes,
    }),
    { count: 0, bytes: 0 },
  );

  return (
    <div className="space">
      <p className="note">{t("space.intro", { tid: lastTid })}</p>

      <section>
        <h2>{t("space.paths")}</h2>
        <nav className="crumbs" aria-label={t("space.pathLabel")}>
          {steps(path).map((step, index, all) =>
            index === all.length - 1 ? (
              <span key={step.path} aria-current="location">
                {step.name}
              </span>
            ) : (
              <span key={step.path}>
                <button type="button" onClick={() => setPath(step.path)}>
                  {step.name}
                </button>
                <span className="muted"> / </span>
              </span>
            ),
          )}
        </nav>
        {pathFailed ? (
          <p className="error">{t("space.pathFailed")}</p>
        ) : pathFound === null ? (
          <Running progress={progress.pathSpace} />
        ) : (
          <>
            <p data-testid="space-path-total">
              {t("space.pathTotal", {
                path: pathFound.path,
                size: formatBytes(pathFound.reach.bytes),
                count: pathFound.reach.count,
              })}
            </p>
            <Treemap
              space={pathFound}
              onOpen={(name) => setPath(child(pathFound.path, name))}
            />
            <p className="note">{t("space.treemapNote")}</p>
          </>
        )}
      </section>

      {spaceFailed ? (
        <p className="error">{t("space.failed")}</p>
      ) : found === null ? (
        <section>
          <h2>{t("space.classes")}</h2>
          <Running progress={progress.space} />
        </section>
      ) : (
        <>
          <section>
            <h2>{t("space.classes")}</h2>
            <p data-testid="space-blobs">
              {t("space.blobs", {
                count: found.blobs.count,
                size: formatBytes(found.blobs.bytes),
              })}
            </p>
            <table className="sizes">
              <thead>
                <tr>
                  <th>{t("space.class")}</th>
                  <th className="number">{t("space.records")}</th>
                  <th className="number">{t("space.bytes")}</th>
                  <th className="number">{t("space.share")}</th>
                </tr>
              </thead>
              <tbody>
                {classes.map(([name, totals]) => (
                  <tr key={name} data-testid="space-class" data-class={name}>
                    <td className="class-name">{name}</td>
                    <td className="number" data-testid="space-count">
                      {totals.count}
                    </td>
                    <td className="number" data-testid="space-bytes">
                      <Bytes bytes={totals.bytes} />
                    </td>
                    <td className="number">{share(totals.bytes, total.bytes)}</td>
                  </tr>
                ))}
              </tbody>
              <tfoot>
                <tr>
                  <td>{t("space.total")}</td>
                  <td className="number">{total.count}</td>
                  <td className="number">
                    <Bytes bytes={total.bytes} />
                  </td>
                  <td />
                </tr>
              </tfoot>
            </table>
          </section>

          <section>
            <h2>{t("space.largest", { count: LARGEST })}</h2>
            <table className="sizes" data-testid="space-largest">
              <thead>
                <tr>
                  <th>{t("object.oid")}</th>
                  <th>{t("space.class")}</th>
                  <th className="number">{t("object.size")}</th>
                </tr>
              </thead>
              <tbody>
                {found.largest.map((large) => (
                  <tr key={large.oid} data-oid={large.oid}>
                    <td>
                      <button
                        type="button"
                        className="oid-link"
                        aria-label={t("space.showObject", { oid: large.oid })}
                        onClick={() => onShow(large.oid)}
                      >
                        {large.oid}
                      </button>
                    </td>
                    <td className="class-name">
                      {large.class ?? (
                        <span className="muted">{t("object.noClass")}</span>
                      )}
                    </td>
                    <td className="number">
                      <Bytes bytes={large.size} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>

          <section>
            <h2>{t("space.growth")}</h2>
            {found.growth.length === 0 ? (
              <p className="note">{t("space.noGrowth")}</p>
            ) : (
              <>
                <p className="note">
                  {t("space.growthNote", { count: found.growth.length })}
                </p>
                <Timeline growth={found.growth} />
              </>
            )}
          </section>
        </>
      )}

      <section>
        <h2>{t("space.pack")}</h2>
        <form className="pack" onSubmit={pack}>
          <label>
            <span>{t("space.packAt")}</span>
            <input
              data-testid="space-reclaim-tid"
              type="text"
              value={packTid}
              placeholder={t("space.packPlaceholder")}
              spellCheck={false}
              onChange={(event) => setPackTid(event.target.value)}
            />
          </label>
          <button data-testid="space-reclaim" type="submit" disabled={reclaiming}>
            {t("space.reclaim")}
          </button>
        </form>
        {reclaiming && <Running progress={undefined} />}
        {reclaim && (
          <>
            <p data-testid="space-reclaim-total">
              {t("space.reclaimTotal", {
                tid: reclaim.tid,
                records: reclaim.records,
                size: formatBytes(reclaim.bytes),
                objects: reclaim.objects,
                blobs: reclaim.blobs,
                blobSize: formatBytes(reclaim.blob_bytes),
              })}
            </p>
            {Object.keys(reclaim.by_class).length > 0 && (
              <table className="sizes" data-testid="space-reclaim-classes">
                <thead>
                  <tr>
                    <th>{t("space.class")}</th>
                    <th className="number">{t("space.records")}</th>
                    <th className="number">{t("space.bytes")}</th>
                  </tr>
                </thead>
                <tbody>
                  {biggest(reclaim.by_class).map(([name, totals]) => (
                    <tr key={name}>
                      <td className="class-name">{name}</td>
                      <td className="number">{totals.count}</td>
                      <td className="number">
                        <Bytes bytes={totals.bytes} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}
      </section>
    </div>
  );
}
