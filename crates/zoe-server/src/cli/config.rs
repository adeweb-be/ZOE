//! The configuration file of `zoe serve`, a TOML file that `--config`
//! (`ZOE_CONFIG`) names.
//!
//! The top-level keys are the options of `zoe serve` without `--`, except
//! `config` and `storage`. A key takes a string, an integer or a boolean,
//! with the syntax of its option. Each `[storages.NAME]` table adds a store
//! that the server serves under the storage name NAME. Its keys are `path`,
//! which is necessary, `blob-dir`, `read-only` and `pack-gc`. The table
//! `[structural-merge-classes]` gives the mode of classes of tier 1, as
//! `"BTrees.IOBTree.IOBucket" = "shadow"`.
//!
//! An option on the command line or in its twin wins over the file. So the
//! file gives only the options that neither of them gives. The stores of
//! the file add to the stores of `--path` and `--storage`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::{ArgMatches, Command};
use toml::{Table, Value};

use super::serve::Storage;

/// The top-level key of the stores.
const STORAGES: &str = "storages";

/// The top-level table of the modes of the classes of tier 1, whose pairs
/// become `--structural-merge-class` options.
const MERGE_CLASSES: &str = "structural-merge-classes";

/// The options of `zoe serve` that are not keys of the file. The
/// `[storages.NAME]` tables take the place of `--storage`, and the table
/// `[structural-merge-classes]` the place of `--structural-merge-class`.
const NOT_KEYS: [&str; 4] = ["config", "storage", "structural_merge_class", "help"];

/// What the configuration file adds to the command line.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Config {
    /// The options that neither the command line nor the twins give, each
    /// as `--KEY=VALUE`.
    pub(super) args: Vec<OsString>,
    /// The stores of the `[storages.NAME]` tables, in the order of their
    /// names.
    pub(super) storages: Vec<Storage>,
}

/// Return the options of the table `[structural-merge-classes]`: one
/// `--structural-merge-class=CLASS=MODE` for each key, in the order of the
/// keys.
fn merge_classes(value: &Value) -> Result<Vec<OsString>, String> {
    let Value::Table(table) = value else {
        return Err(format!("{MERGE_CLASSES} is a table of CLASS = MODE"));
    };
    table
        .iter()
        .map(|(class, mode)| match mode {
            Value::String(mode) => {
                let pair = format!("{class}={mode}");
                zoe_core::merge::class_mode(&pair)
                    .map(|_| format!("--structural-merge-class={pair}").into())
                    .map_err(|error| format!("{MERGE_CLASSES}: {error}"))
            }
            _ => Err(format!("{MERGE_CLASSES}.{class} takes on, shadow or off")),
        })
        .collect()
}

/// Read the configuration file `path` of the command `serve`. `matches`
/// are the arguments of the command line and of the twins.
pub(super) fn read(path: &Path, serve: &Command, matches: &ArgMatches) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("the configuration file {}: {error}", path.display()))?;
    parse(&text, serve, matches)
        .map_err(|error| format!("the configuration file {}: {error}", path.display()))
}

/// Parse the text of a configuration file.
fn parse(text: &str, serve: &Command, matches: &ArgMatches) -> Result<Config, String> {
    let table: Table = text
        .parse()
        .map_err(|error: toml::de::Error| error.to_string())?;
    let mut config = Config::default();
    for (key, value) in &table {
        if key == STORAGES {
            config.storages = storages(value)?;
            continue;
        }
        if key == MERGE_CLASSES {
            let given = matches.value_source("structural_merge_class");
            let classes = merge_classes(value)?;
            if !matches!(
                given,
                Some(ValueSource::CommandLine | ValueSource::EnvVariable)
            ) {
                config.args.extend(classes);
            }
            continue;
        }
        let arg = serve
            .get_arguments()
            .filter(|arg| !NOT_KEYS.contains(&arg.get_id().as_str()))
            .find(|arg| arg.get_long() == Some(key.as_str()))
            .ok_or_else(|| format!("{key} is not a key of the file"))?;
        let value = match value {
            Value::String(text) => text.clone(),
            Value::Integer(n) => n.to_string(),
            Value::Boolean(flag) => flag.to_string(),
            _ => return Err(format!("{key} takes a string, an integer or a boolean")),
        };
        let given = matches.value_source(arg.get_id().as_str());
        if !matches!(
            given,
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        ) {
            config.args.push(format!("--{key}={value}").into());
        }
    }
    Ok(config)
}

/// Return the stores of the `[storages.NAME]` tables.
fn storages(value: &Value) -> Result<Vec<Storage>, String> {
    let tables = value
        .as_table()
        .ok_or_else(|| format!("{STORAGES} takes a [{STORAGES}.NAME] table for each store"))?;
    tables
        .iter()
        .map(|(name, value)| storage(name, value))
        .collect()
}

/// Return the store of the table `[storages.NAME]`.
fn storage(name: &str, value: &Value) -> Result<Storage, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("{STORAGES}.{name} is not a table"))?;
    if name.is_empty() {
        return Err(format!("a table of {STORAGES} has an empty name"));
    }
    let mut path = None;
    let mut blob_dir = None;
    let mut read_only = false;
    let mut pack_gc = true;
    for (key, value) in table {
        let boolean = || {
            value
                .as_bool()
                .ok_or_else(|| format!("{STORAGES}.{name}.{key} takes true or false"))
        };
        match key.as_str() {
            "path" => path = Some(directory(name, key, value)?),
            "blob-dir" => blob_dir = Some(directory(name, key, value)?),
            "read-only" => read_only = boolean()?,
            "pack-gc" => pack_gc = boolean()?,
            _ => return Err(format!("{key} is not a key of {STORAGES}.{name}")),
        }
    }
    let path = path.ok_or_else(|| format!("{STORAGES}.{name} has no path"))?;
    Ok(Storage {
        name: name.to_owned(),
        path,
        blob_dir,
        read_only,
        pack_gc,
    })
}

/// Return the directory that the key `key` of `[storages.NAME]` gives. It
/// is a string that is not empty.
fn directory(name: &str, key: &str, value: &Value) -> Result<PathBuf, String> {
    match value.as_str() {
        Some(text) if !text.is_empty() => Ok(PathBuf::from(text)),
        _ => Err(format!("{STORAGES}.{name}.{key} takes a directory")),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::super::Cli;
    use super::*;

    /// Parse `text` as the configuration file of `zoe serve` with `args`.
    fn parse_with(text: &str, args: &[&str]) -> Result<Config, String> {
        let command = Cli::command();
        let line = ["zoe", "serve"].iter().chain(args);
        let matches = command.clone().try_get_matches_from(line).unwrap();
        let serve = command.find_subcommand("serve").unwrap();
        parse(text, serve, matches.subcommand_matches("serve").unwrap())
    }

    fn args(config: &Config) -> Vec<&str> {
        config
            .args
            .iter()
            .map(|arg| arg.to_str().unwrap())
            .collect()
    }

    #[test]
    fn the_keys_become_options_of_the_command_line() {
        let config = parse_with(
            "path = \"store\"\nlisten = \"127.0.0.1:0\"\nlock-timeout = 2\n\
             max-frame-size = 1024\n",
            &[],
        )
        .unwrap();
        assert_eq!(
            args(&config),
            [
                "--listen=127.0.0.1:0",
                "--lock-timeout=2",
                "--max-frame-size=1024",
                "--path=store"
            ]
        );
        assert!(config.storages.is_empty());
    }

    #[test]
    fn blob_dir_and_shared_blob_dir_are_keys_of_the_file() {
        let config = parse_with("blob-dir = \"b\"\nshared-blob-dir = true\n", &[]).unwrap();
        assert_eq!(args(&config), ["--blob-dir=b", "--shared-blob-dir=true"]);
        let given = ["--blob-dir", "a", "--shared-blob-dir=false"];
        let config = parse_with("blob-dir = \"b\"\nshared-blob-dir = true\n", &given).unwrap();
        assert!(config.args.is_empty());
    }

    #[test]
    fn an_option_of_the_command_line_wins_over_its_key() {
        let config = parse_with(
            "listen = \"192.0.2.1:1\"\nlock-timeout = \"2m\"\n",
            &["--listen", "127.0.0.1:0"],
        )
        .unwrap();
        assert_eq!(args(&config), ["--lock-timeout=2m"]);
    }

    #[test]
    fn the_tables_give_the_stores_in_the_order_of_their_names() {
        let config = parse_with(
            "[storages.main]\npath = \"m\"\nread-only = false\n\n\
             [storages.catalog]\npath = \"c\"\nblob-dir = \"cb\"\n",
            &[],
        )
        .unwrap();
        assert!(config.args.is_empty());
        assert_eq!(
            config.storages,
            [
                Storage {
                    name: "catalog".into(),
                    path: "c".into(),
                    blob_dir: Some("cb".into()),
                    read_only: false,
                    pack_gc: true,
                },
                Storage {
                    name: "main".into(),
                    path: "m".into(),
                    blob_dir: None,
                    read_only: false,
                    pack_gc: true,
                },
            ]
        );
        assert_eq!(parse_with("[storages]\n", &[]).unwrap(), Config::default());
    }

    #[test]
    fn read_only_makes_one_store_read_only() {
        let config = parse_with(
            "[storages.archive]\npath = \"a\"\nread-only = true\n\n\
             [storages.live]\npath = \"l\"\n",
            &[],
        )
        .unwrap();
        let read_only: Vec<(&str, bool)> = config
            .storages
            .iter()
            .map(|storage| (storage.name.as_str(), storage.read_only))
            .collect();
        assert_eq!(read_only, [("archive", true), ("live", false)]);
    }

    #[test]
    fn pack_gc_false_packs_one_store_without_gc() {
        let config = parse_with(
            "[storages.kept]\npath = \"k\"\npack-gc = false\n\n\
             [storages.main]\npath = \"m\"\npack-gc = true\n\n\
             [storages.other]\npath = \"o\"\n",
            &[],
        )
        .unwrap();
        let pack_gc: Vec<(&str, bool)> = config
            .storages
            .iter()
            .map(|storage| (storage.name.as_str(), storage.pack_gc))
            .collect();
        assert_eq!(pack_gc, [("kept", false), ("main", true), ("other", true)]);
    }

    #[test]
    fn a_bad_file_is_refused() {
        for (text, error) in [
            ("[storages.main\n", "TOML parse error"),
            ("no-such-key = 1\n", "no-such-key is not a key of the file"),
            ("config = \"other.toml\"\n", "config is not a key"),
            ("storage = \"a=b\"\n", "storage is not a key"),
            ("help = true\n", "help is not a key"),
            ("listen = 1.5\n", "listen takes a string"),
            ("listen = [\"a\"]\n", "listen takes a string"),
            ("storages = 1\n", "storages takes a [storages.NAME] table"),
            ("[storages]\nmain = 1\n", "storages.main is not a table"),
            (
                "[storages.main]\nread-only = false\n",
                "storages.main has no path",
            ),
            ("[storages.main]\npath = \"\"\n", "storages.main.path takes"),
            ("[storages.main]\npath = 1\n", "storages.main.path takes"),
            (
                "[storages.main]\npath = \"m\"\nsize = 1\n",
                "size is not a key",
            ),
            (
                "[storages.main]\npath = \"m\"\nread-only = 1\n",
                "takes true or false",
            ),
            (
                "[storages.main]\npath = \"m\"\npack-gc = \"no\"\n",
                "storages.main.pack-gc takes true or false",
            ),
            ("[storages.\"\"]\npath = \"m\"\n", "empty name"),
        ] {
            let found = parse_with(text, &[]).unwrap_err();
            assert!(found.contains(error), "{text:?}: {found}");
        }
    }
}
