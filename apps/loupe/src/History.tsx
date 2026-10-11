// The history of one object: its revisions, newest first, and the diff of
// two of them. The operator picks the revision "from" and the revision
// "to" in the rows. A button in each row sets the "as of" switch to the
// tid of the row.

import { useEffect, useState } from "react";

import { type Change, diff, HISTORY_LIMIT, history, type Revision } from "./api";
import { formatState } from "./format";
import { t } from "./i18n";

/** The most changes that the diff view shows. */
const CHANGE_LIMIT = 1_000;

/** The most characters of one value of a change that the view shows. */
const VALUE_LIMIT = 20_000;

interface Props {
  oid: string;
  /** The serial of the revision that the pane shows. */
  serial: string;
  onAsOf: (tid: string) => void;
  onError: (failure: unknown) => void;
}

export function History({ oid, serial, onAsOf, onError }: Props) {
  const [revisions, setRevisions] = useState<Revision[] | null>(null);
  const [historyFailed, setHistoryFailed] = useState(false);
  const [from, setFrom] = useState<string | null>(null);
  const [to, setTo] = useState<string | null>(null);
  const [changes, setChanges] = useState<Change[] | null>(null);
  const [diffFailed, setDiffFailed] = useState(false);

  // Each effect drops a reply that comes after the oid changes. The
  // parent gives a new `onError` at each render, so no effect depends on
  // it.
  useEffect(() => {
    let live = true;
    setRevisions(null);
    setHistoryFailed(false);
    history(oid).then(
      (found) => live && setRevisions(found),
      (failure) => {
        if (live) {
          setHistoryFailed(true);
          onError(failure);
        }
      },
    );
    return () => {
      live = false;
    };
  }, [oid]);

  useEffect(() => {
    setChanges(null);
    setDiffFailed(false);
    if (from === null || to === null) {
      return;
    }
    let live = true;
    diff(oid, from, to).then(
      (found) => live && setChanges(found),
      (failure) => {
        if (live) {
          setDiffFailed(true);
          onError(failure);
        }
      },
    );
    return () => {
      live = false;
    };
  }, [oid, from, to]);

  return (
    <>
      <h2>{t("history.title")}</h2>
      {historyFailed ? (
        <p className="error">{t("history.failed")}</p>
      ) : revisions === null ? (
        <p>{t("history.loading")}</p>
      ) : (
        <>
          {revisions.length >= HISTORY_LIMIT && (
            <p className="note">
              {t("history.limit", { count: HISTORY_LIMIT })}
            </p>
          )}
          <table className="history">
            <thead>
              <tr>
                <th>{t("history.from")}</th>
                <th>{t("history.to")}</th>
                <th>{t("history.tid")}</th>
                <th>{t("history.user")}</th>
                <th>{t("history.description")}</th>
                <th>{t("history.size")}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {revisions.map((revision) => {
                const shown = revision.tid === serial;
                return (
                  <tr
                    key={revision.tid}
                    data-testid="history-row"
                    className={shown ? "shown" : undefined}
                    aria-current={shown ? "true" : undefined}
                  >
                    <td>
                      <input
                        type="radio"
                        name="diff-from"
                        aria-label={t("history.fromLabel", { tid: revision.tid })}
                        checked={from === revision.tid}
                        onChange={() => setFrom(revision.tid)}
                      />
                    </td>
                    <td>
                      <input
                        type="radio"
                        name="diff-to"
                        aria-label={t("history.toLabel", { tid: revision.tid })}
                        checked={to === revision.tid}
                        onChange={() => setTo(revision.tid)}
                      />
                    </td>
                    <td className="tid">{revision.tid}</td>
                    <td>
                      {revision.user === "" ? (
                        <span className="muted">{t("history.noUser")}</span>
                      ) : (
                        revision.user
                      )}
                    </td>
                    <td>{revision.description}</td>
                    <td>
                      {revision.size === 0
                        ? t("history.deleted")
                        : t("object.sizeBytes", { size: revision.size })}
                    </td>
                    <td>
                      <button
                        type="button"
                        aria-label={t("history.showLabel", { tid: revision.tid })}
                        onClick={() => onAsOf(revision.tid)}
                      >
                        {t("history.show")}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </>
      )}
      {from !== null && to !== null && (
        <DiffView from={from} to={to} changes={changes} failed={diffFailed} />
      )}
    </>
  );
}

interface DiffProps {
  from: string;
  to: string;
  changes: Change[] | null;
  /** True when the diff call failed. */
  failed: boolean;
}

/** The changes from the revision `from` to the revision `to`. */
function DiffView({ from, to, changes, failed }: DiffProps) {
  return (
    <section className="diff" data-testid="diff">
      <h3>{t("diff.title", { from, to })}</h3>
      {failed ? (
        <p className="error">{t("diff.failed")}</p>
      ) : changes === null ? (
        <p>{t("diff.loading")}</p>
      ) : changes.length === 0 ? (
        <p>{t("diff.none")}</p>
      ) : (
        <>
          {changes.length > CHANGE_LIMIT && (
            <p className="note">
              {t("diff.limit", {
                shown: CHANGE_LIMIT,
                count: changes.length,
              })}
            </p>
          )}
          <table className="changes">
            <thead>
              <tr>
                <th>{t("diff.path")}</th>
                <th>{t("diff.before")}</th>
                <th>{t("diff.after")}</th>
              </tr>
            </thead>
            <tbody>
              {changes.slice(0, CHANGE_LIMIT).map((change) => (
                <ChangeRow key={change.path} change={change} />
              ))}
            </tbody>
          </table>
        </>
      )}
    </section>
  );
}

/** One change: its path, and the value before and after. */
function ChangeRow({ change }: { change: Change }) {
  const before = "before" in change;
  const after = "after" in change;
  // A value that the state cuts or leaves out can change in a part that
  // does not show. Then the two sides look the same.
  const hidden =
    before &&
    after &&
    JSON.stringify(change.before) === JSON.stringify(change.after);
  return (
    <tr data-testid="diff-change">
      <td className="at">
        {change.path === "" ? t("diff.whole") : change.path}
        {hidden && <p className="note">{t("diff.hidden")}</p>}
      </td>
      <td>
        <Value present={before} value={change.before} />
      </td>
      <td>
        <Value present={after} value={change.after} />
      </td>
    </tr>
  );
}

/** One value of a change, as the pane writes a state. */
function Value({ present, value }: { present: boolean; value: unknown }) {
  if (!present) {
    return <span className="muted">{t("diff.absent")}</span>;
  }
  const formatted = formatState(value, VALUE_LIMIT);
  return (
    <>
      <pre className="value">{formatted.text}</pre>
      {!formatted.complete && <p className="note">{t("diff.partial")}</p>}
    </>
  );
}
