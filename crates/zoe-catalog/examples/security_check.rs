//! Resolve the entries that `security_check.py` exports and compare each
//! value with the value of the indexer of Plone.
//!
//!     cargo run --release -p zoe-catalog --example security_check -- entries.tsv
//!
//! A line is `E` (an entry) or `O` (a container outside the catalog), the
//! path, the kind, the roles, the local roles as `principal=role|role;...`,
//! the block flag, and for an entry its value from the indexer.

use std::time::Instant;

use zoe_catalog::security::{Classes, Inputs, Kind, SecurityIndex};

fn main() {
    let path = std::env::args().nth(1).expect("the path of entries.tsv");
    let text = std::fs::read_to_string(path).expect("read entries.tsv");
    let mut index = SecurityIndex::new();
    let mut entries = Vec::new();
    let started = Instant::now();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let kind = match fields[2] {
            "-" => Kind::Missing,
            "t" => Kind::Tuple,
            "l" => Kind::List,
            "a" => Kind::Anonymous,
            "v" => Kind::Opaque,
            other => panic!("unknown kind {other}"),
        };
        let roles = split(fields[3], ',').map(|r| index.intern(r)).collect();
        let local = split(fields[4], ';')
            .map(|pair| {
                let (principal, roles) = pair.split_once('=').expect("principal=roles");
                let principal = index.intern(principal);
                let roles = split(roles, '|').map(|r| index.intern(r)).collect();
                (principal, roles)
            })
            .collect();
        let inputs = Inputs {
            kind,
            roles,
            local,
            block: fields[5] == "1",
        };
        let node = index.set(fields[1], inputs);
        if fields[0] == "E" {
            entries.push((node, fields[6].to_owned()));
        }
    }
    let loaded = started.elapsed();
    let started = Instant::now();
    let values = index.resolve();
    let resolved = started.elapsed();
    let mut differ = 0;
    for (node, wanted) in &entries {
        let mut got: Vec<&str> = values[*node].iter().map(|&id| index.name(id)).collect();
        got.sort_unstable();
        if got.join(",") != *wanted {
            differ += 1;
            if differ <= 5 {
                eprintln!("differs: node {node}: got {got:?}, wanted {wanted}");
            }
        }
    }
    let of_entries: Vec<&Vec<u32>> = entries.iter().map(|(node, _)| &values[*node]).collect();
    let started = Instant::now();
    let classes = Classes::new(&of_entries);
    let grouped = started.elapsed();
    let anonymous = index.intern("Anonymous");
    let started = Instant::now();
    let found = classes.query(&[anonymous]);
    let queried = started.elapsed();
    let matches: u32 = found.iter().map(|word| word.count_ones()).sum();
    println!(
        "entries {} nodes {} differ {} classes {} anonymous {} load {:?} resolve {:?} group {:?} query {:?}",
        entries.len(),
        index.len(),
        differ,
        classes.values.len(),
        matches,
        loaded,
        resolved,
        grouped,
        queried,
    );
    if differ > 0 {
        std::process::exit(1);
    }
}

fn split(text: &str, separator: char) -> impl Iterator<Item = &str> {
    text.split(separator).filter(|part| !part.is_empty())
}
