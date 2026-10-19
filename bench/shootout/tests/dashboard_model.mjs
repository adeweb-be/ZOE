// Print the model of the dashboard of a checkout of the results repository.
//
//     node dashboard_model.mjs CHECKOUT
//
// The script imports CHECKOUT/data.js and calls its `load` with a reader of
// the files of CHECKOUT. It prints one JSON object on stdout: `requested`,
// the paths that `load` asked for, in their order, and `model`, the value
// that `load` returned.

import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const checkout = process.argv[2];
const requested = [];

async function read(path) {
  requested.push(path);
  return JSON.parse(await readFile(join(checkout, path), "utf8"));
}

const { load } = await import(pathToFileURL(join(checkout, "data.js")).href);
const model = await load(read);
process.stdout.write(JSON.stringify({ requested, model }));
