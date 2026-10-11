// Acceptance test of time travel in the window, through tauri-driver. The
// test opens the fixture store of the zoe-loupe tests, goes to a document,
// sees its history, switches "as of" to an older tid and sees the older
// value, then switches back. It also calls the IPC commands that the window
// uses.
//
// The test needs the built app, tauri-driver and WebKitWebDriver.

import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { $, $$, browser, expect } from "@wdio/globals";

const REPO = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const DOC = "/plone/folder/doc";

/** An object as the IPC commands return it. */
interface LoupeObject {
  oid: string;
  serial: string;
  class: string | null;
  state: Record<string, unknown>;
}

/** One revision of an object, newest first. */
interface Revision {
  tid: string;
  user: string;
  description: string;
  size: number;
}

/** One change of a diff. */
interface Change {
  path: string;
  before: unknown;
  after: unknown;
}

/** Write the fixture store of the zoe-loupe tests into a new directory. */
function fixtureStore(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "loupe-e2e-"));
  execFileSync(
    "cargo",
    [
      "test",
      "--locked",
      "--quiet",
      "--package",
      "zoe-loupe",
      "--test",
      "browse",
      "--",
      "--exact",
      "the_fixture_store_opens",
    ],
    {
      cwd: REPO,
      env: { ...process.env, ZOE_LOUPE_FIXTURE: dir },
      stdio: "inherit",
    },
  );
  return dir;
}

/** Call an IPC command of the app from the window. */
async function invoke<T>(
  command: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  return (await browser.execute(
    (command: string, args: Record<string, unknown>) =>
      (window as any).__TAURI_INTERNALS__.invoke(command, args),
    command,
    args,
  )) as T;
}

/** Type a tid into the "as of" switch, or nothing for the latest tid. */
async function asOf(tid: string): Promise<void> {
  const input = $('[data-testid="as-of"]');
  await input.clearValue();
  if (tid) {
    await input.setValue(tid);
  }
  await browser.keys("Enter");
}

describe("time travel", () => {
  let doc: LoupeObject;
  let history: Revision[];

  before(async () => {
    const store = fixtureStore();
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();
    await $('[data-testid="object-path"]').setValue(DOC);
    await browser.keys("Enter");
    doc = await invoke<LoupeObject>("resolve", { path: DOC });
    history = await invoke<Revision[]>("history", { oid: doc.oid });
  });

  it("lists the history of an object", async () => {
    expect(history.map((r) => `${r.user} ${r.description}`)).toEqual([
      "editor annotate the document",
      "editor edit the document",
      "someone create the site",
    ]);
    expect(history[0].tid).toBe(doc.serial);

    const rows = $$('[data-testid="history-row"]');
    await expect(rows).toBeElementsArrayOfSize(3);
    await expect(rows[0]).toHaveText(
      expect.stringContaining("annotate the document"),
    );
    await expect(rows[2]).toHaveText(
      expect.stringContaining("create the site"),
    );
  });

  it("shows an older value as of an older tid", async () => {
    const first = history[2].tid;
    await asOf(first);
    await expect($('[data-testid="object-serial"]')).toHaveText(first);
    await expect($('[data-testid="object-state"]')).toHaveText(
      expect.stringContaining("Doc, first"),
    );

    await asOf("");
    await expect($('[data-testid="object-serial"]')).toHaveText(doc.serial);
    await expect($('[data-testid="object-state"]')).toHaveText(
      expect.stringContaining("Doc, second"),
    );
  });

  it("reads every object as of the tid of the switch", async () => {
    const first = history[2].tid;
    await invoke("set_as_of", { tid: first });
    const old = await invoke<LoupeObject>("resolve", { path: DOC });
    expect(old.serial).toBe(first);
    expect(old.state.title).toBe("Doc, first");
    const folder = await invoke<LoupeObject>("resolve", {
      path: "/plone/folder",
    });
    expect(folder.state.title).toBe("Folder");

    await invoke("set_as_of", { tid: null });
    const latest = await invoke<LoupeObject>("resolve", { path: DOC });
    expect(latest.serial).toBe(doc.serial);
  });

  it("diffs the decoded states of two revisions", async () => {
    const changes = await invoke<Change[]>("diff", {
      oid: doc.oid,
      tidA: history[2].tid,
      tidB: history[1].tid,
    });
    expect(changes).toEqual([
      { path: "/title", before: "Doc, first", after: "Doc, second" },
    ]);
  });
});
