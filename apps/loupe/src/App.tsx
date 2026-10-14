// The window of Loupe: the folder bar with the "as of" switch and the
// screens. The object browser shows the object tree and the object pane.
// The space screen measures the store.

import { useEffect, useRef, useState } from "react";

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
import { SpaceScreen } from "./Space";

/** A screen of the window. */
type Screen = "browser" | "space";

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
  const [screen, setScreen] = useState<Screen>("browser");
  // The open that the space screen measures. The screen walks the store
  // only once the operator shows it, and it stays mounted afterwards, so
  // that a move between the screens keeps its results.
  const [spaceOpen, setSpaceOpen] = useState<number | null>(null);

  useEffect(() => {
    if (screen === "space") {
      setSpaceOpen(opens);
    }
  }, [screen, opens]);

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

  const open = (path: string) =>
    run(async (current) => {
      setFolder(null);
      setRootObject(null);
      setRootMissing(false);
      setCurrent(null);
      const opened = await openFolder(path);
      // The Rust side has the folder open, with the switch on the last
      // transaction. The window shows this even when a newer request
      // started. A move counts only once its call succeeds, so a failed
      // call drops no root.
      const move = ++latestMove.current;
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

  /** Show the object `oid` in the object browser. */
  const showInBrowser = (oid: string) => {
    setScreen("browser");
    void show(oid);
  };

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
    const before = current;
    const shown = before?.oid ?? null;
    // When a newer request stops the travel, the pane drops the object
    // from before the move. An object that a newer request put in the
    // pane stays.
    const stop = () =>
      setCurrent((pane) => (pane === before ? null : pane));
    return run(async (current) => {
      const tid = text.trim() === "" ? null : text.trim();
      await setAsOf(tid);
      // The switch of the Rust side has moved. The window shows the move
      // even when a newer request started, so it never shows two tids. A
      // move counts only once its call succeeds, so a failed call drops no
      // root.
      const move = ++latestMove.current;
      setAsOfTid(tid);
      setAsOfText(tid ?? "");
      setRootObject(null);
      setRootMissing(false);
      setViews((count) => count + 1);
      if (!current()) {
        stop();
      }
      // A view before the first transaction has no root. The tree then
      // shows a note.
      const [foundRoot, rootFailure] = await reloadRoot(move);
      let failure = rootFailure;
      if (!current()) {
        stop();
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
        stop();
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
        {folder && (
          <nav className="screens" role="tablist" aria-label={t("screen.label")}>
            {(["browser", "space"] as const).map((name) => (
              <button
                key={name}
                type="button"
                role="tab"
                data-testid={`screen-${name}`}
                aria-selected={screen === name}
                onClick={() => setScreen(name)}
              >
                {t(name === "browser" ? "screen.browser" : "screen.space")}
              </button>
            ))}
          </nav>
        )}
      </header>
      {error && (
        <p className="error" role="alert">
          {t("error.shown", { message: error })}
        </p>
      )}
      {folder && spaceOpen === opens && (
        <div className="screen" hidden={screen !== "space"}>
          <SpaceScreen
            key={opens}
            lastTid={folder.lastTid}
            onShow={showInBrowser}
            onError={(failure) => setError(describe(failure))}
          />
        </div>
      )}
      {folder ? (
        <main hidden={screen !== "browser"}>
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
