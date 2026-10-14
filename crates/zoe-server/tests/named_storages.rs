//! Acceptance test of the options of named storages and of the server
//! configuration file.
//!
//! `--storage NAME=PATH` adds a named storage and repeats. `--path PATH` is
//! the storage named `1`. `--config FILE` (`ZOE_CONFIG`) reads a TOML file:
//! its top-level keys are the options of `zoe serve` without `--`, and its
//! `[storages.NAME]` tables give the stores, with the keys `path`,
//! `blob-dir` and `read-only` of each store. An option on the command line
//! wins over the file, as it wins over its twin. A storage name that comes
//! twice, a storage without a path, an unknown key and a file that does not
//! parse are usage errors: the command exits with 2 and creates nothing.

use std::process::{Command, Output, Stdio};

mod wire;

use wire::{Server, ZOE, commit, fresh_dir, load, new_oids, record, s, spawn_lock};

fn zoe(args: &[&str]) -> Output {
    let mut command = Command::new(ZOE);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("ZOE_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = {
        let _spawn = spawn_lock();
        command.spawn().unwrap()
    };
    child.wait_with_output().unwrap()
}

#[test]
fn the_help_names_the_options_and_their_twins() {
    let output = zoe(&["serve", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for name in ["--storage", "ZOE_STORAGE", "--config", "ZOE_CONFIG"] {
        assert!(help.contains(name), "{name} is not in:\n{help}");
    }
}

#[test]
fn storages_come_from_the_command_line() {
    let dir = fresh_dir("cli");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let server = Server::start(
        &["--storage", &format!("a={}", s(&a)), "--path", s(&b)],
        &[],
    );
    let mut first = server.client("a");
    let mut second = server.client("1");
    let oid = new_oids(&mut first, 1)[0];
    let tid = commit(&mut first, 1, &[(oid, 0, record(b"a"))]).unwrap();
    assert_eq!(load(&mut first, oid).unwrap(), (record(b"a"), tid));
    assert!(load(&mut second, oid).is_err());
}

#[test]
fn storages_and_options_come_from_the_configuration_file() {
    let dir = fresh_dir("config");
    let config = dir.join("zoe.toml");
    std::fs::write(
        &config,
        format!(
            "lock-timeout = 2\n\
             \n\
             [storages.main]\n\
             path = \"{}\"\n\
             \n\
             [storages.catalog]\n\
             path = \"{}\"\n\
             blob-dir = \"{}\"\n",
            s(&dir.join("main")),
            s(&dir.join("catalog")),
            s(&dir.join("catalog-blobs")),
        ),
    )
    .unwrap();
    let server = Server::start(&["--config", s(&config)], &[]);
    for name in ["main", "catalog"] {
        let mut conn = server.client(name);
        let oid = new_oids(&mut conn, 1)[0];
        commit(&mut conn, 1, &[(oid, 0, record(name.as_bytes()))]).unwrap();
    }
    assert!(dir.join("main").join("zoe.redb").exists());
    assert!(dir.join("catalog").join("zoe.redb").exists());
}

#[test]
fn the_command_line_wins_over_the_file() {
    let dir = fresh_dir("override");
    let config = dir.join("zoe.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"192.0.2.1:1\"\n[storages.main]\npath = \"{}\"\n",
            s(&dir.join("main"))
        ),
    )
    .unwrap();
    // The listen address of the file cannot be bound on this host. The one
    // of the command line wins over it.
    let server = Server::start(&["--config", s(&config), "--listen", "127.0.0.1:0"], &[]);
    server.client("main");
}

#[test]
fn bad_storage_settings_are_usage_errors_and_create_nothing() {
    let dir = fresh_dir("usage");
    let store = dir.join("store");
    let twice = format!("x={}", s(&store));
    let cases: Vec<Vec<String>> = vec![
        // The same name twice.
        vec![
            "--storage".into(),
            twice.clone(),
            "--storage".into(),
            twice.clone(),
        ],
        // The name 1 from --path and from --storage.
        vec![
            "--path".into(),
            s(&store).into(),
            "--storage".into(),
            format!("1={}", s(&store)),
        ],
        // No name.
        vec!["--storage".into(), s(&store).into()],
    ];
    for args in cases {
        let mut all = vec!["serve".to_string()];
        all.extend(args.clone());
        all.extend(["--listen".into(), "127.0.0.1:0".into()]);
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        let output = zoe(&all);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }

    for (name, text) in [
        ("unknown-key", "no-such-key = 1\n".to_string()),
        ("no-path", "[storages.main]\nread-only = true\n".to_string()),
        ("no-parse", "[storages.main\n".to_string()),
    ] {
        let config = dir.join(format!("{name}.toml"));
        std::fs::write(&config, text).unwrap();
        let output = zoe(&["serve", "--config", s(&config), "--listen", "127.0.0.1:0"]);
        assert_eq!(output.status.code(), Some(2), "{name}");
    }
    assert!(!store.exists(), "a usage error created the store");
}
