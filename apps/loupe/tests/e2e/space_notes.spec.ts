// Acceptance test of the notes of the space screen, through tauri-driver.
// The size of a path and the growth count the records only. The screen
// says that they leave the blob files out: the blob files have a line of
// their own in the table of the classes.
//
// The test needs the built app, tauri-driver and WebKitWebDriver.

import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { $, expect } from "@wdio/globals";

const REPO = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);

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

describe("the notes of the space screen", () => {
  before(async () => {
    const store = fixtureStore();
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();
    await $('[data-testid="screen-space"]').click();
  });

  it("says that the size of a path leaves the blob files out", async () => {
    await expect($('[data-testid="space-path-total"]')).toHaveText(
      expect.stringMatching(/blob/i),
    );
  });

  it("says that the growth leaves the blob files out", async () => {
    await expect($('[data-testid="space-growth"]')).toBeDisplayed();
    await expect($('[data-testid="space-growth-note"]')).toHaveText(
      expect.stringMatching(/blob/i),
    );
  });
});
