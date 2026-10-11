// The window of Loupe: the folder bar with the "as of" switch, the object
// tree and the object pane.

import { useRef, useState } from "react";

import {
  describe,
  type FolderInfo,
  type LoupeObject,
  object,
  openFolder,
  pickFolder,
  resolve,
  root,
  setAsOf,
} from "./api";
import { t } from "./i18n";
import { ObjectPane } from "./ObjectPane";
import { ObjectTree } from "./ObjectTree";

export function App() {
  const [folderPath, setFolderPath] = useState("");
  const [folder, setFolder] = useState<FolderInfo | null>(null);
  const [rootObject, setRootObject] = useState<LoupeObject | null>(null);
  // True when the view of the switch has no root.
  const [rootMissing, setRootMissing] = useState(false);
  const [current, setCurrent] = useState<LoupeObject | null>(null);
  const [objectPath, setObjectPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  // The tid of the "as of" switch, or null for the last transaction, and
  // the text of its field.
  const [asOf, setAsOfTid] = useState<string | null>(null);
  const [asOfText, setAsOfText] = useState("");
  // Each open gets a new history pane.
  const [opens, setOpens] = useState(0);
  // Each open and each move of the switch get a new tree, with nothing
  // loaded.
  const [views, setViews] = useState(0);
  // The number of the last request. A reply to an older request comes
  // too late, and the window drops it.
  const latest = useRef(0);
  // The number of the last move of the switch. A root that comes after a
  // newer move is too late, and the window drops it.
  const latestMove = useRef(0);

  /** Run an IPC call. Show its error, unless a newer request started. */
  const run = async (call: (current: () => boolean) => Promise<void>) => {
    const ticket = ++latest.current;
    const current = () => ticket === latest.current;
    setError(null);
    try {
      await call(current);
    } catch (failure) {
      if (current()) {
        setError(describe(failure));
      }
    }
  };

  /**
   * Read the root again after a move of the switch. Keep it only when no
   * newer move came. Return the root, or null with the failure.
   */
  const reloadRoot = async (
    move: number,
  ): Promise<[LoupeObject | null, unknown]> => {
    let found: LoupeObject | null = null;
    let failure: unknown = null;
    try {
      found = await root();
    } catch (rootFailure) {
      failure = rootFailure;
    }
    if (move === latestMove.current) {
      setRootObject(found);
      setRootMissing(found === null);
    }
    return [found, failure];
  };

  const open = (path: string) => {
    const move = ++latestMove.current;
    return run(async (current) => {
      setFolder(null);
      setRootObject(null);
      setRootMissing(false);
      setCurrent(null);
      const opened = await openFolder(path);
      // The Rust side has the folder open, with the switch on the last
      // transaction. The window shows this even when a newer request
      // started.
      setFolder(opened);
      setOpens((count) => count + 1);
      setViews((count) => count + 1);
      setAsOfTid(null);
      setAsOfText("");
      const [found, failure] = await reloadRoot(move);
      if (!current()) {
        return;
      }
      setCurrent(found);
      setObjectPath("");
      if (failure !== null) {
        throw failure;
      }
    });
  };

  const browse = async () => {
    try {
      const picked = await pickFolder();
      if (picked !== null) {
        setFolderPath(picked);
        await open(picked);
      }
    } catch (failure) {
      setError(describe(failure));
    }
  };

  const show = (oid: string) =>
    run(async (current) => {
      const found = await object(oid);
      if (current()) {
        setCurrent(found);
      }
    });

  const go = (path: string) =>
    run(async (current) => {
      const found = await resolve(path);
      if (current()) {
        setCurrent(found);
      }
    });

  /**
   * Set the "as of" switch to `text`, a tid, or to the last transaction
   * when `text` is empty. Then read the root and the shown object again.
   */
  const travel = (text: string) => {
    const shown = current?.oid ?? null;
    const move = ++latestMove.current;
    return run(async (current) => {
      const tid = text.trim() === "" ? null : text.trim();
      await setAsOf(tid);
      // The switch of the Rust side has moved. The window shows the move
      // even when a newer request started, so it never shows two tids.
      setAsOfTid(tid);
      setAsOfText(tid ?? "");
      setRootObject(null);
      setRootMissing(false);
      setViews((count) => count + 1);
      if (!current()) {
        // A newer request fills the pane, as of this move or a later one.
        setCurrent(null);
      }
      // A view before the first transaction has no root. The tree then
      // shows a note.
      const [foundRoot, rootFailure] = await reloadRoot(move);
      let failure = rootFailure;
      if (!current()) {
        return;
      }
      // The pane keeps the object until its new revision comes. An object
      // with no record as of the tid leaves the pane empty.
      let found: LoupeObject | null = foundRoot;
      try {
        if (shown !== null) {
          found = await object(shown);
        }
      } catch (objectFailure) {
        found = null;
        failure = objectFailure;
      }
      if (!current()) {
        return;
      }
      setCurrent(found);
      if (failure !== null) {
        throw failure;
      }
    });
  };

  return (
    <div className="app">
      <header className="folder">
        <h1>{t("app.title")}</h1>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            void open(folderPath);
          }}
        >
          <label>
            <span>{t("folder.label")}</span>
            <input
              data-testid="folder-path"
              type="text"
              value={folderPath}
              placeholder={t("folder.placeholder")}
              spellCheck={false}
              onChange={(event) => setFolderPath(event.target.value)}
            />
          </label>
          <button data-testid="open-folder" type="submit">
            {t("folder.open")}
          </button>
          <button type="button" onClick={() => void browse()}>
            {t("folder.browse")}
          </button>
        </form>
        {folder && (
          <p className="status">
            {t("folder.opened", { path: folder.path, tid: folder.lastTid })}
          </p>
        )}
        {folder && (
          <div className="as-of">
            <label>
              <span>{t("asOf.label")}</span>
              <input
                data-testid="as-of"
                type="text"
                value={asOfText}
                placeholder={t("asOf.placeholder")}
                spellCheck={false}
                onChange={(event) => setAsOfText(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    void travel(event.currentTarget.value);
                  }
                }}
                // The field also applies a new value when it loses the
                // focus.
                onBlur={(event) => {
                  if (event.currentTarget.value.trim() !== (asOf ?? "")) {
                    void travel(event.currentTarget.value);
                  }
                }}
              />
            </label>
            <span className="status">
              {asOf === null
                ? t("asOf.latest")
                : t("asOf.shown", { tid: asOf })}
            </span>
          </div>
        )}
      </header>
      {error && (
        <p className="error" role="alert">
          {t("error.shown", { message: error })}
        </p>
      )}
      {folder ? (
        <main>
          {rootObject ? (
            <ObjectTree
              key={views}
              root={rootObject}
              selected={current?.oid ?? null}
              onSelect={(oid) => void show(oid)}
              onError={(failure) => setError(describe(failure))}
            />
          ) : rootMissing ? (
            <p className="tree empty">
              {t("asOf.noRoot", { tid: asOf ?? folder.lastTid })}
            </p>
          ) : (
            <div className="tree" />
          )}
          <ObjectPane
            object={current}
            store={opens}
            path={objectPath}
            onPathChange={setObjectPath}
            onGo={(path) => void go(path)}
            onSelect={(oid) => void show(oid)}
            onAsOf={(tid) => void travel(tid)}
            onError={(failure) => setError(describe(failure))}
          />
        </main>
      ) : (
        <p className="empty">{t("folder.none")}</p>
      )}
    </div>
  );
}
