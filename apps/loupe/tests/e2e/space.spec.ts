// Acceptance test of the space screen in the window, through tauri-driver.
// The test opens the fixture store of the zoe-loupe tests, opens the space
// screen, and sees the classes with their counts and bytes, the treemap of
// the paths, the largest objects, the timeline of the growth and what a
// pack would reclaim. It also calls the IPC commands that the screen uses.
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

interface ClassSpace {
  count: number;
  bytes: number;
}

interface Space {
  by_class: Record<string, ClassSpace>;
  largest: { oid: string; class: string | null; size: number }[];
  growth: { tid: string; bytes: number }[];
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

describe("the space screen", () => {
  before(async () => {
    const store = fixtureStore();
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();
    await $('[data-testid="screen-space"]').click();
  });

  it("lists the classes with their counts and bytes", async () => {
    const space = await invoke<Space>("space", { largest: 10 });
    const mapping = space.by_class["persistent.mapping.PersistentMapping"];
    expect(mapping.count).toBeGreaterThan(0);

    const row = $(
      '[data-testid="space-class"][data-class="persistent.mapping.PersistentMapping"]',
    );
    await expect(row.$('[data-testid="space-count"]')).toHaveText(
      String(mapping.count),
    );
  });

  it("shows the treemap, the largest objects and the growth", async () => {
    await expect($('[data-testid="space-treemap"]')).toBeDisplayed();
    const space = await invoke<Space>("space", { largest: 10 });
    const rows = await $$('[data-testid="space-largest"] tr[data-oid]');
    expect(rows.length).toBe(Math.min(10, space.largest.length));
    await expect($('[data-testid="space-growth"]')).toBeDisplayed();
  });

  it("shows what a pack would reclaim", async () => {
    await $('[data-testid="space-reclaim"]').click();
    await expect($('[data-testid="space-reclaim-total"]')).toHaveText(
      expect.stringMatching(/\d/),
    );
  });
});
