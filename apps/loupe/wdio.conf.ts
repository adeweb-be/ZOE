// The WebdriverIO configuration of the end-to-end tests. tauri-driver
// starts the built app and drives its window through WebKitWebDriver.
// `just loupe e2e` builds the app and runs these tests under xvfb-run.

import { type ChildProcess, spawn } from "node:child_process";
import { connect, createServer, type AddressInfo } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** The app binary. `LOUPE_APP` overrides the debug build of the recipe. */
const APPLICATION =
  process.env.LOUPE_APP ??
  path.resolve(HERE, "../../target/loupe/debug/loupe");

/** Return a TCP port that no server uses now. */
async function freePort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const { port } = server.address() as AddressInfo;
  await new Promise((done) => server.close(done));
  return port;
}

// The launcher picks the ports. The workers inherit its environment, so
// they use the same ports.
process.env.LOUPE_DRIVER_PORT ??= String(await freePort());
process.env.LOUPE_NATIVE_PORT ??= String(await freePort());
const PORT = Number(process.env.LOUPE_DRIVER_PORT);
const NATIVE_PORT = Number(process.env.LOUPE_NATIVE_PORT);

let driver: ChildProcess | undefined;

/** Wait until a server listens on `port`. */
async function listening(port: number): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt++) {
    const open = await new Promise<boolean>((done) => {
      const socket = connect(port, "127.0.0.1");
      socket.once("connect", () => {
        socket.end();
        done(true);
      });
      socket.once("error", () => done(false));
    });
    if (open) {
      return;
    }
    await new Promise((wait) => setTimeout(wait, 100));
  }
  throw new Error(`nothing listens on port ${port}`);
}

/**
 * Stop tauri-driver, WebKitWebDriver and the app. tauri-driver leads a
 * process group of its own, and the other two are in it. Signal the group
 * also when tauri-driver has stopped, because the app can outlive it.
 */
function stopDriver(): void {
  if (driver?.pid !== undefined) {
    try {
      process.kill(-driver.pid, "SIGTERM");
    } catch {
      // No process of the group is left.
    }
  }
  driver = undefined;
}

export const config: WebdriverIO.Config = {
  runner: "local",
  hostname: "127.0.0.1",
  port: PORT,
  specs: ["./tests/e2e/**/*.spec.ts"],
  maxInstances: 1,
  capabilities: [
    {
      "tauri:options": { application: APPLICATION },
    } as WebdriverIO.Capabilities,
  ],
  logLevel: "warn",
  reporters: ["spec"],
  framework: "mocha",
  mochaOpts: { ui: "bdd", timeout: 300_000 },
  waitforTimeout: 10_000,

  beforeSession: async () => {
    driver = spawn(
      "tauri-driver",
      ["--port", String(PORT), "--native-port", String(NATIVE_PORT)],
      { detached: true, stdio: ["ignore", "inherit", "inherit"] },
    );
    await listening(PORT);
  },

  afterSession: () => stopDriver(),
};

// A worker that stops early still stops the app.
process.once("exit", stopDriver);
