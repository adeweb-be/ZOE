//! Tests of the configuration file of `zoe serve` beside its acceptance
//! test: the twin of an option wins over the file, and the stores of the
//! file add to the stores of the command line, and the twin and the key
//! of `--read-only`.

mod wire;

use wire::{Server, ZOE, commit, fresh_dir, load, new_oids, open_store, record, s};

#[test]
fn the_twin_wins_over_the_file() {
    let dir = fresh_dir("config-twin");
    let config = dir.join("zoe.toml");
    let path = format!("path = \"{}\"\n", s(&dir.join("store")));
    // The option refuses a frame size of 0. So the server starts only
    // when the twin wins over the file.
    std::fs::write(&config, format!("max-frame-size = 0\n{path}")).unwrap();
    let server = Server::start(
        &["--config", s(&config)],
        &[("ZOE_MAX_FRAME_SIZE", "1048576")],
    );
    server.client("1");
    drop(server);

    // ZOE_CONFIG names the file, and --config wins over it.
    std::fs::write(&config, path).unwrap();
    let server = Server::start(&[], &[("ZOE_CONFIG", s(&config))]);
    server.client("1");
    drop(server);
    let server = Server::start(
        &["--config", s(&config)],
        &[("ZOE_CONFIG", "/no/such/file.toml")],
    );
    server.client("1");
}

#[test]
fn the_stores_of_the_file_add_to_the_stores_of_the_command_line() {
    let dir = fresh_dir("config-add");
    let config = dir.join("zoe.toml");
    std::fs::write(
        &config,
        format!(
            "[storages.catalog]\npath = \"{}\"\nread-only = false\n",
            s(&dir.join("catalog"))
        ),
    )
    .unwrap();
    let server = Server::start(
        &[
            "--config",
            s(&config),
            "--path",
            s(&dir.join("one")),
            "--storage",
            &format!("main={}", s(&dir.join("main"))),
        ],
        &[],
    );
    let mut conns: Vec<_> = ["1", "main", "catalog"]
        .into_iter()
        .map(|name| server.client(name))
        .collect();
    // Each store gives the same first oid, and holds only its own record.
    let oids: Vec<u64> = conns.iter_mut().map(|c| new_oids(c, 1)[0]).collect();
    assert!(oids.iter().all(|&oid| oid == oids[0]), "{oids:?}");
    for (n, conn) in conns.iter_mut().enumerate() {
        let data = record(&[b'a' + n as u8]);
        let tid = commit(conn, 1, &[(oids[0], 0, data.clone())]).unwrap();
        assert_eq!(load(conn, oids[0]).unwrap(), (data, tid));
    }
    for name in ["one", "main", "catalog"] {
        assert!(dir.join(name).join("zoe.redb").exists(), "{name}");
    }
}

#[test]
fn the_twin_and_the_key_of_read_only_make_every_store_read_only() {
    let dir = fresh_dir("config-read-only");
    let store = dir.join("store");
    drop(open_store(&store));
    let config = dir.join("zoe.toml");
    std::fs::write(&config, "read-only = true\n").unwrap();
    for (args, env) in [
        (vec!["--path", s(&store)], vec![("ZOE_READ_ONLY", "true")]),
        (vec!["--path", s(&store), "--config", s(&config)], vec![]),
    ] {
        let server = Server::start(&args, &env);
        let mut conn = server.connect();
        let fault = conn.register("1", false).unwrap_err();
        assert_eq!(fault.class, "ZODB.POSException.ReadOnlyError", "{args:?}");
        conn.register("1", true).unwrap();
    }
}

#[test]
fn a_read_only_store_with_a_blob_directory_is_a_usage_error() {
    let dir = fresh_dir("config-read-only-blob-dir");
    let store = dir.join("store");
    drop(open_store(&store));
    let config = dir.join("zoe.toml");
    let table = format!(
        "[storages.a]\npath = \"{}\"\nblob-dir = \"{}\"\n",
        s(&store),
        s(&dir.join("blobs"))
    );
    for (text, flag) in [
        (format!("{table}read-only = true\n"), None),
        (table.clone(), Some("--read-only")),
    ] {
        std::fs::write(&config, text).unwrap();
        let mut command = std::process::Command::new(ZOE);
        command.args(["serve", "--config", s(&config), "--listen", "127.0.0.1:0"]);
        command.args(flag).env_remove("ZOE_READ_ONLY");
        let output = {
            let _spawn = wire::spawn_lock();
            command.output().unwrap()
        };
        assert_eq!(output.status.code(), Some(2), "{flag:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("takes no blob directory"), "{stderr}");
    }
    assert!(!dir.join("blobs").exists());
}
