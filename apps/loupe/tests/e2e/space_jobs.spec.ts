// Acceptance test of the jobs of the space screen in the window, through
// tauri-driver. The window gives each walk a job id, as the argument
// `jobId` of `space`, `path_space` and `reclaimable`. Each `job-progress`
// event of the walk carries that id with the name of its command, so the
// window shows only the progress of the walk that it waits for. A new walk
// of the same command stops the walk before it: the zoe-loupe test
// `crates/zoe-loupe/tests/jobs.rs` checks the stop, because a walk of the
// fixture store ends before a second call can reach it.
//
// The test does not open the space screen, so the window starts no walk
// of its own.
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

/** The payload of a `job-progress` event. */
interface JobProgress {
  job: string;
  id: number;
  walk: string;
  done: number;
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

/**
 * Keep the payload of each `job-progress` event in `window.__jobProgress`.
 * This is what `listen` of `@tauri-apps/api/event` does.
 */
async function collectProgress(): Promise<void> {
  await browser.execute(() => {
    const tauri = (window as any).__TAURI_INTERNALS__;
    (window as any).__jobProgress = [];
    return tauri.invoke("plugin:event|listen", {
      event: "job-progress",
      target: { kind: "Any" },
      handler: tauri.transformCallback((event: { payload: unknown }) => {
        (window as any).__jobProgress.push(event.payload);
      }),
    });
  });
}

/** Return the progress events that came since `collectProgress`. */
async function progressEvents(): Promise<JobProgress[]> {
  return (await browser.execute(
    () => (window as any).__jobProgress,
  )) as JobProgress[];
}

/** Wait for a progress event of the command `job` with the id `id`. */
async function waitForEvent(job: string, id: number): Promise<void> {
  await browser.waitUntil(
    async () =>
      (await progressEvents()).some((event) => event.job === job && event.id === id),
    { timeoutMsg: `no progress event of ${job} with the id ${id}` },
  );
}

describe("the jobs of the space screen", () => {
  before(async () => {
    const store = fixtureStore();
    await $('[data-testid="folder-path"]').setValue(store);
    await $('[data-testid="open-folder"]').click();
    await expect($('[data-testid="object-oid"]')).toHaveText("0x00");
    await collectProgress();
  });

  it("puts the job id of a path walk into its progress events", async () => {
    const measured = await invoke<{ path: string }>("path_space", {
      path: "/plone",
      jobId: 42,
    });
    expect(measured.path).toBe("/plone");
    await waitForEvent("pathSpace", 42);
    const events = (await progressEvents()).filter(
      (event) => event.job === "pathSpace",
    );
    expect(events.every((event) => event.id === 42)).toBe(true);
  });

  it("puts the job id of the walk of the classes into its progress events", async () => {
    await invoke("space", { largest: 10, jobId: 43 });
    await waitForEvent("space", 43);
  });
});
