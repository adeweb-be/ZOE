// The data of the dashboard. This is a plain ES module with no access to
// the DOM, so node can import it too.
//
// load(read) reads index.json, then each document of the index, and returns
// the model of the page as plain JSON data. read(path) returns a promise of
// the parsed JSON of a file of the site, by a path relative to the page.
//
// The model holds only numbers of the documents, with one exception: the
// ratio of each number of a B5 step to the number of zeo in the same step.

// The stack that each ratio of a B5 table compares to.
export const BASELINE = "zeo";

// The numbers of a row of a B5 table, by their name in a step.
export const COLUMNS = [
  "ok_per_second",
  "p50_seconds",
  "p95_seconds",
  "p99_seconds",
  "conflicts_per_1000_ops",
  "first_attempt_share",
  "exhausted",
  "zope_cpu_ms_per_op",
  "server_cpu_ms_per_op",
  "store_write_kb_per_op",
  "objects_loaded_per_second",
  "objects_stored_per_second",
];

// The B5 scenarios that get a chart of users against the rate and the
// conflicts.
export const CURVES = ["edit", "create-spread"];

// The families of the tables. The page shows the latest document of each
// group, and a selector gives the earlier ones.
export const SELECTED = ["native", "client", "plone"];

// The keys of a document that the method of the page shows.
const METHOD = {
  shootout: ["machine", "python", "software", "run"],
  native: ["machine", "python", "software", "run", "rule"],
  client: ["machine", "python", "software", "run"],
  plone: [
    "machine",
    "python",
    "software",
    "run",
    "fixture",
    "settings",
    "stacks",
    "scenarios",
  ],
};

export async function load(read) {
  const index = await read("index.json");
  // An entry of an index of version 1 has no family and no stacks: its
  // document is of zodbshootout, and its storage is its stack.
  const entries = (index.results ?? []).map((entry) => ({
    ...entry,
    family: entry.family ?? "shootout",
    stacks: entry.stacks ?? (entry.storage === undefined ? [] : [entry.storage]),
  }));
  const sources = await Promise.all(entries.map((entry) => read(entry.path)));
  const documents = entries.map((entry, at) => describe(entry, sources[at]));
  return {
    documents,
    shootout: shootoutCharts(entries, sources),
    selection: selection(entries),
  };
}

// Compare two strings by their code units, as Python compares them.
function compare(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

function describe(entry, source) {
  const document = { ...entry, method: {} };
  for (const key of METHOD[entry.family] ?? []) {
    document.method[key] = source[key];
  }
  if (entry.family === "plone") {
    document.tables = ploneTables(source);
    document.series = ploneSeries(source);
  } else if (entry.family === "native") {
    document.cells = nativeCells(source);
  } else if (entry.family === "client") {
    document.rows = source.rows;
  }
  return document;
}

function stepValue(step, name) {
  const number = name === "ok_per_second" ? step.ok_per_second?.median : step[name];
  return number ?? null;
}

function ratio(number, base) {
  if (number === null || base === null || base === 0) return null;
  return number / base;
}

// One table per scenario and step, with one row per stack that ran the
// step, in the order of the stacks of the document.
function ploneTables(source) {
  const order = source.stacks.map((stack) => stack.name);
  const tables = [];
  for (const scenario of source.scenarios) {
    const steps = source.steps.filter((step) => step.scenario === scenario.name);
    const numbers = [...new Set(steps.map((step) => step.step))].sort((a, b) => a - b);
    for (const number of numbers) {
      const ran = new Map();
      for (const step of steps) {
        if (step.step === number && !ran.has(step.stack)) ran.set(step.stack, step);
      }
      const base = ran.get(BASELINE);
      const rows = order
        .filter((name) => ran.has(name))
        .map((name) => {
          const step = ran.get(name);
          const values = {};
          const ratios = {};
          for (const column of COLUMNS) {
            values[column] = stepValue(step, column);
            ratios[column] = ratio(values[column], base ? stepValue(base, column) : null);
          }
          return {
            stack: name,
            values,
            ratios,
            range: {
              min: step.ok_per_second?.min ?? null,
              max: step.ok_per_second?.max ?? null,
            },
            p99_reliable: step.p99_reliable ?? null,
            verification: source.runs
              .filter((run) => run.scenario === scenario.name && run.stack === name)
              .map((run) => run.verification),
          };
        });
      tables.push({
        scenario: scenario.name,
        step: number,
        users: ran.values().next().value.users,
        rows,
      });
    }
  }
  return tables;
}

// One series per stack and scenario of CURVES: a point per step, in the
// order of the users.
function ploneSeries(source) {
  const series = [];
  for (const scenario of CURVES) {
    for (const stack of source.stacks) {
      const points = source.steps
        .filter((step) => step.scenario === scenario && step.stack === stack.name)
        .map((step) => ({
          users: step.users,
          ok_per_second: stepValue(step, "ok_per_second"),
          conflicts_per_1000_ops: stepValue(step, "conflicts_per_1000_ops"),
        }))
        .sort((a, b) => a.users - b.users);
      if (points.length) series.push({ scenario, stack: stack.name, points });
    }
  }
  return series;
}

// One entry per cell and scenario. The result of a scenario comes from the
// document: the page applies no rule of the gate.
function nativeCells(source) {
  const cells = [];
  for (const cell of source.cells) {
    for (const scenario of cell.scenarios) {
      let verdict = null;
      if ("verdict" in scenario) verdict = scenario.verdict ?? "undecided";
      cells.push({
        mode: cell.mode,
        concurrency: cell.concurrency,
        object_size: cell.object_size,
        objects_per_transaction: cell.objects_per_transaction,
        scenario: scenario.name,
        ratio: scenario.ratio ?? null,
        low: scenario.low ?? null,
        high: scenario.high ?? null,
        verdict,
      });
    }
  }
  return cells;
}

// One chart per machine, Python build, cell and scenario of zodbshootout,
// over every run: a line per storage, a point per document.
function shootoutCharts(entries, sources) {
  const charts = new Map();
  entries.forEach((entry, at) => {
    if (entry.family !== "shootout") return;
    const source = sources[at];
    for (const scenario of source.scenarios) {
      const key = JSON.stringify([
        entry.machine,
        entry.python,
        scenario.mode,
        scenario.concurrency,
        scenario.object_size,
        scenario.name,
      ]);
      if (!charts.has(key)) {
        charts.set(key, {
          machine: entry.machine,
          python: entry.python,
          mode: scenario.mode,
          concurrency: scenario.concurrency,
          object_size: scenario.object_size,
          scenario: scenario.name,
          lines: [],
        });
      }
      const chart = charts.get(key);
      let line = chart.lines.find((item) => item.storage === source.storage);
      if (!line) {
        line = { storage: source.storage, points: [] };
        chart.lines.push(line);
      }
      line.points.push({
        started: source.run.started,
        commit: source.run.commit,
        ops_per_second: scenario.ops_per_second,
        p50_seconds: scenario.p50_seconds,
        p99_seconds: scenario.p99_seconds,
      });
    }
  });
  for (const chart of charts.values()) {
    for (const line of chart.lines) {
      line.points.sort((a, b) => compare(a.started, b.started));
    }
  }
  return [...charts.values()];
}

// One group per family of SELECTED, machine, Python build and, for plone,
// fixture: the newest document and the others, newest first.
function selection(entries) {
  const groups = new Map();
  for (const entry of entries) {
    if (!SELECTED.includes(entry.family)) continue;
    const fixture = entry.family === "plone" ? (entry.fixture ?? null) : null;
    const key = JSON.stringify([entry.family, entry.machine, entry.python, fixture]);
    if (!groups.has(key)) {
      groups.set(key, {
        family: entry.family,
        machine: entry.machine,
        python: entry.python,
        fixture,
        members: [],
      });
    }
    groups.get(key).members.push(entry);
  }
  return [...groups.values()].map(({ members, ...group }) => {
    const newest = members
      .slice()
      .sort((a, b) => compare(b.started, a.started) || compare(b.path, a.path))
      .map((entry) => entry.path);
    return { ...group, latest: newest[0], earlier: newest.slice(1) };
  });
}
