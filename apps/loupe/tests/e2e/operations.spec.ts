// Acceptance test of the operations screen in the window, through
// tauri-driver. The test opens the fixture store of the zoe-loupe tests,
// runs verify, which finds nothing, and starts a pack: the window shows
// the dry run of the pack and runs the pack only after the confirmation.
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

describe("the operations screen", () => {
  before(async () => {
    const store = fixtureStore();
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();
    await $('[data-testid="screen-operations"]').click();
  });

  it("runs verify and shows no finding", async () => {
    await $('[data-testid="operation-verify"]').click();
    await $('[data-testid="confirm"]').click();
    await expect($('[data-testid="verify-result"]')).toHaveAttribute(
      "data-clean",
      "true",
    );
  });

  it("shows the dry run before a pack, and packs on confirmation", async () => {
    await $('[data-testid="operation-pack"]').click();
    await expect($('[data-testid="pack-dry-run"]')).toBeDisplayed();
    await $('[data-testid="cancel"]').click();
    await expect($('[data-testid="pack-result"]')).not.toBeDisplayed();

    await $('[data-testid="operation-pack"]').click();
    await expect($('[data-testid="pack-dry-run"]')).toBeDisplayed();
    await $('[data-testid="confirm"]').click();
    await expect($('[data-testid="pack-result"]')).toBeDisplayed();
  });
});
