// Acceptance test of the object browser in the window, through tauri-driver.
// The test opens the fixture store of the zoe-loupe tests, sees the root,
// goes to a path of the site, and sees the class and the oid of the object.
// It also calls the IPC commands that the window uses.
//
// The test needs the built app, tauri-driver and WebKitWebDriver.

import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { $, browser, expect } from "@wdio/globals";

const REPO = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);

/** An object as the IPC commands return it. */
interface LoupeObject {
  oid: string;
  serial: string;
  class: string | null;
  size: number;
  state: unknown;
  references: { oid: string; at: string }[];
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

/** Type a path into the path bar and go there. */
async function goTo(objectPath: string): Promise<void> {
  await $('[data-testid="object-path"]').setValue(objectPath);
  await browser.keys("Enter");
}

describe("the object browser", () => {
  let store: string;

  before(() => {
    store = fixtureStore();
  });

  it("opens a store folder and shows the root", async () => {
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();

    await expect($('[data-testid="object-class"]')).toHaveText(
      "persistent.mapping.PersistentMapping",
    );
    await expect($('[data-testid="object-oid"]')).toHaveText("0x00");

    const root = await invoke<LoupeObject>("root");
    expect(root.oid).toBe("0x00");
    expect(root.class).toBe("persistent.mapping.PersistentMapping");
    await expect($('[data-testid="object-serial"]')).toHaveText(root.serial);
  });

  it("follows a reference from the root", async () => {
    const root = await invoke<LoupeObject>("root");
    const reference = root.references.find((r) => r.at === "/data/Application");
    expect(reference).toBeDefined();
    const app = await invoke<LoupeObject>("object", { oid: reference!.oid });
    expect(app.oid).toBe(reference!.oid);
    expect(app.class).toBe("app.content.Application");
  });

  it("goes to a path and shows the class and the oid", async () => {
    const doc = await invoke<LoupeObject>("resolve", {
      path: "/plone/folder/doc",
    });
    expect(doc.class).toBe("app.content.Document");
    expect(doc.oid).toMatch(/^0x[0-9a-f]+$/);

    await goTo("/plone/folder/doc");
    await expect($('[data-testid="object-class"]')).toHaveText(
      "app.content.Document",
    );
    await expect($('[data-testid="object-oid"]')).toHaveText(doc.oid);
    await expect($('[data-testid="object-serial"]')).toHaveText(doc.serial);
    await expect($('[data-testid="object-state"]')).toHaveText(
      expect.stringContaining("Doc, second"),
    );
  });

  it("goes to a path through annotations", async () => {
    await goTo("/plone/folder/doc/__annotations__/app.settings");
    await expect($('[data-testid="object-class"]')).toHaveText(
      "persistent.mapping.PersistentMapping",
    );
  });
});
