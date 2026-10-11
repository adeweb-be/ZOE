// The window of Loupe: the folder bar, the object tree and the object
// pane.

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
} from "./api";
import { t } from "./i18n";
import { ObjectPane } from "./ObjectPane";
import { ObjectTree } from "./ObjectTree";

export function App() {
  const [folderPath, setFolderPath] = useState("");
  const [folder, setFolder] = useState<FolderInfo | null>(null);
  const [rootObject, setRootObject] = useState<LoupeObject | null>(null);
  const [current, setCurrent] = useState<LoupeObject | null>(null);
  const [objectPath, setObjectPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  // Each open gets a new tree, with nothing loaded.
  const [opens, setOpens] = useState(0);
  // The number of the last request. A reply to an older request comes
  // too late, and the window drops it.
  const latest = useRef(0);

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

  const open = (path: string) =>
    run(async (current) => {
      setFolder(null);
      setRootObject(null);
      setCurrent(null);
      const opened = await openFolder(path);
      const found = await root();
      if (!current()) {
        return;
      }
      setFolder(opened);
      setOpens((count) => count + 1);
      setRootObject(found);
      setCurrent(found);
      setObjectPath("");
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

  const go = (path: string) =>
    run(async (current) => {
      const found = await resolve(path);
      if (current()) {
        setCurrent(found);
      }
    });

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
      </header>
      {error && (
        <p className="error" role="alert">
          {t("error.shown", { message: error })}
        </p>
      )}
      {rootObject ? (
        <main>
          <ObjectTree
            key={opens}
            root={rootObject}
            selected={current?.oid ?? null}
            onSelect={(oid) => void show(oid)}
            onError={(failure) => setError(describe(failure))}
          />
          <ObjectPane
            object={current}
            path={objectPath}
            onPathChange={setObjectPath}
            onGo={(path) => void go(path)}
            onSelect={(oid) => void show(oid)}
          />
        </main>
      ) : (
        <p className="empty">{t("folder.none")}</p>
      )}
    </div>
  );
}
