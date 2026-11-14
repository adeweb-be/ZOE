//! The Markdown report of a result of `zoe-bench catalog`. It shows the
//! numbers of the result and computes none.

use std::fmt::Write;

use serde_json::Value as Json;

/// The Markdown report of the result `doc`.
pub fn markdown(doc: &Json) -> String {
    let mut out = String::new();
    let catalog = &doc["catalog"];
    let _ = writeln!(out, "# zoe-bench catalog: {}\n", text(&catalog["name"]));
    let _ = writeln!(
        out,
        "Preset {}, {} entries. Machine {}. Started {}, commit {}, dirty {}.\n",
        text(&catalog["preset"]),
        text(&catalog["counts"]["size"]),
        text(&doc["machine"]["name"]),
        text(&doc["run"]["started"]),
        text(&doc["run"]["commit"]),
        text(&doc["run"]["dirty"]),
    );
    let server = &doc["server"];
    let _ = writeln!(
        out,
        "Server: catalog-memory {}, mirror {} bytes (the line of ADR 0059: {} bytes, over it: {}), build {} s, RSS {} bytes, peak {} bytes.\n",
        text(&server["catalog_memory"]),
        text(&server["mirror_bytes"]),
        text(&server["adr_0059_line_bytes"]),
        text(&server["over_adr_line"]),
        text(&server["build_seconds"]),
        text(&server["rss_bytes"]),
        text(&server["peak_rss_bytes"]),
    );
    let scrapes = &doc["metrics"]["scrapes"];
    let _ = writeln!(
        out,
        "/metrics every second: {} scrapes, p50 {}, p99 {}, max {}.\n",
        text(&scrapes["count"]),
        ms(&scrapes["p50_seconds"]),
        ms(&scrapes["p99_seconds"]),
        ms(&scrapes["max_seconds"]),
    );
    for scenario in doc["scenarios"].as_array().into_iter().flatten() {
        let name = text(&scenario["name"]);
        let _ = writeln!(out, "## {name}\n");
        let _ = writeln!(
            out,
            "Mirror {} bytes, build {} s, RSS {} bytes, peak {} bytes.\n",
            text(&scenario["mirror_bytes"]),
            text(&scenario["build_seconds"]),
            text(&scenario["rss_bytes"]),
            text(&scenario["peak_rss_bytes"]),
        );
        match name.as_str() {
            "cold" => {
                for key in [
                    "listening_seconds",
                    "mirror_ready_seconds",
                    "first_answer_seconds",
                    "text_ready_seconds",
                    "page_cache_dropped_bytes",
                ] {
                    let _ = writeln!(out, "- {key}: {}", text(&scenario[key]));
                }
                let _ = writeln!(
                    out,
                    "- refusals before the answers: {}",
                    scenario["refusals"]
                );
                spread(&mut out, "first answers", &scenario["first_answers"]);
                hub(&mut out, "during the build", &scenario["hub_during_build"]);
                out.push('\n');
            }
            "text" => {
                for mode in scenario["modes"].as_array().into_iter().flatten() {
                    let _ = writeln!(
                        out,
                        "### mode {}\n\nText index ready after {} s; filter of `text_query`: {} rids; commits: {}.\n",
                        text(&mode["mode"]),
                        text(&mode["text_ready_seconds"]),
                        text(&mode["filter_rids"]),
                        mode["commits"],
                    );
                    table(&mut out, &mode["cells"]);
                }
            }
            "text-rebuild" => {
                for key in [
                    "mode",
                    "rebuild_seconds",
                    "mirror_ready_seconds",
                    "rss_at_mirror_ready_bytes",
                    "peak_rss_after_mirror_ready_bytes",
                    "rebuild_after_mirror_ready",
                    "peak_rss_with_mirror_build_bytes",
                ] {
                    let _ = writeln!(out, "- {key}: {}", text(&scenario[key]));
                }
                out.push('\n');
            }
            _ => {}
        }
        if name == "mix" {
            let _ = writeln!(
                out,
                "Native share {} of {} lines; client reasons {}; server reasons {}.\n",
                text(&scenario["native_share"]),
                text(&scenario["lines"]),
                scenario["client_reasons"],
                scenario["server_reasons"],
            );
        }
        if scenario["cells"].is_array() {
            table(&mut out, &scenario["cells"]);
        }
        let page = &scenario["page"];
        if page.is_object() {
            let _ = writeln!(
                out,
                "Full page: {} rows of {} columns, date columns {}, {} reply bytes.\n",
                text(&page["rows"]),
                text(&page["columns"]),
                page["date_columns"],
                text(&page["reply_bytes"]),
            );
            table(&mut out, &page["cells"]);
            spread(
                &mut out,
                "client decode of the page",
                &page["client_decode"],
            );
            spread(
                &mut out,
                "client encode of the page",
                &page["client_encode"],
            );
            out.push('\n');
        }
        let overrides = &scenario["overrides"];
        if overrides.is_object() {
            let _ = writeln!(
                out,
                "Path queries over {} overridden rows ({} writes):\n",
                text(&overrides["override_rows"]),
                text(&overrides["writes"]),
            );
            table(&mut out, &overrides["cells"]);
        }
        let rebase = &scenario["rebase"];
        if rebase.is_object() {
            hub(&mut out, "past the rebase threshold", &rebase["hub"]);
            out.push('\n');
        }
    }
    out
}

fn text(value: &Json) -> String {
    match value {
        Json::Null => "-".into(),
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn ms(value: &Json) -> String {
    value
        .as_f64()
        .map_or_else(|| "-".into(), |s| format!("{:.3} ms", s * 1000.0))
}

fn table(out: &mut String, cells: &Json) {
    let _ = writeln!(
        out,
        "| cell | conc. | queries | answered | p50 | p99 | max | q/s | refusals | errors |"
    );
    let _ = writeln!(
        out,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    for cell in cells.as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} |",
            text(&cell["name"]),
            text(&cell["concurrency"]),
            text(&cell["queries"]),
            text(&cell["answered"]),
            ms(&cell["p50_seconds"]),
            ms(&cell["p99_seconds"]),
            ms(&cell["max_seconds"]),
            cell["qps"].as_f64().unwrap_or(0.0),
            cell["refusals"],
            cell["errors"],
        );
    }
    out.push('\n');
    for cell in cells.as_array().into_iter().flatten() {
        let Some(queries) = cell["by_query"].as_array() else {
            continue;
        };
        let _ = writeln!(out, "Each query of the cell {}:\n", text(&cell["name"]));
        let _ = writeln!(out, "| query | answered | p50 | p99 | max |");
        let _ = writeln!(out, "| --- | --- | --- | --- | --- |");
        for q in queries {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                text(&q["name"]),
                text(&q["count"]),
                ms(&q["p50_seconds"]),
                ms(&q["p99_seconds"]),
                ms(&q["max_seconds"]),
            );
        }
        out.push('\n');
    }
}

fn spread(out: &mut String, what: &str, value: &Json) {
    let _ = writeln!(
        out,
        "- {what}: {} samples, p50 {}, p99 {}, max {}",
        text(&value["count"]),
        ms(&value["p50_seconds"]),
        ms(&value["p99_seconds"]),
        ms(&value["max_seconds"]),
    );
}

fn hub(out: &mut String, when: &str, value: &Json) {
    if value.is_null() {
        return;
    }
    spread(
        out,
        &format!(
            "invalidation delay {when} ({} commits)",
            text(&value["commits"])
        ),
        &value["invalidation_delay"],
    );
}
