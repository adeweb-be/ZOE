//! TLS of the client against `zoe serve` in the test process, with real
//! certificates.
//!
//! The client always checks the chain of the server certificate against
//! `authenticate`: a file, a directory, or the CA certificates of the
//! system when it has none. With `check_hostname`, it also checks the name.
//! A failed check fails the open, and the client never speaks without TLS.
//!
//! The test makes its certificates with the `openssl` command, as the TLS
//! tests of `zoe-server` do, so the repository holds no key.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use zoe_client::{Client, Endpoint, Error, Options, Tls, TlsOptions, Tokio};
use zoe_server::serve::{ServeOptions, Server};

/// The extensions of a self-signed CA certificate, without spaces.
const CA: &str = "-addext basicConstraints=critical,CA:TRUE \
                  -addext keyUsage=critical,keyCertSign,cRLSign,digitalSignature";

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-client-tls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `openssl` with the arguments `args`, split at the spaces, in `dir`.
/// Fail the test when it fails.
fn openssl(dir: &Path, args: &str) {
    let args: Vec<&str> = args.split_whitespace().collect();
    let output = Command::new("openssl")
        .args(&args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "openssl {args:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The arguments of a new P-256 key without a password.
const NEW_KEY: &str = "-newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes";

/// Make a self-signed CA certificate `NAME.pem` with its key `NAME.key`.
fn self_signed(dir: &Path, name: &str) {
    openssl(
        dir,
        &format!(
            "req -x509 {NEW_KEY} -days 2 -subj /CN={name} -keyout {name}.key -out {name}.pem {CA}"
        ),
    );
}

/// Make the certificate `NAME.pem` with the extensions `extensions` and
/// its key `NAME.key`, signed by the key of the CA `ca`.
fn signed(dir: &Path, name: &str, ca: &str, extensions: &str) {
    std::fs::write(dir.join(format!("{name}.ext")), extensions).unwrap();
    openssl(
        dir,
        &format!("req {NEW_KEY} -subj /CN={name} -keyout {name}.key -out {name}.csr"),
    );
    openssl(
        dir,
        &format!(
            "x509 -req -in {name}.csr -CA {ca}.pem -CAkey {ca}.key -CAserial {ca}.srl \
             -CAcreateserial -days 2 -extfile {name}.ext -out {name}.pem"
        ),
    );
}

/// Return the directory of the certificates of the test. The first call
/// makes them.
///
/// - `server-ca` signs `server`, valid for `localhost` and `127.0.0.1`.
/// - `client-ca` signs `client`.
/// - `other-ca` signs nothing that the test uses.
fn certs() -> &'static Path {
    static CERTS: OnceLock<PathBuf> = OnceLock::new();
    CERTS.get_or_init(|| {
        let dir = fresh_dir("certs");
        for name in ["server-ca", "client-ca", "other-ca"] {
            self_signed(&dir, name);
        }
        signed(
            &dir,
            "server",
            "server-ca",
            "basicConstraints = critical, CA:FALSE\n\
             extendedKeyUsage = serverAuth\n\
             subjectAltName = DNS:localhost, IP:127.0.0.1\n",
        );
        signed(
            &dir,
            "client",
            "client-ca",
            "basicConstraints = critical, CA:FALSE\n\
             extendedKeyUsage = clientAuth\n",
        );
        dir
    })
}

/// Start a server with TLS that requires a certificate of `client-ca`.
fn serve(name: &str) -> Server {
    let certs = certs();
    let tls =
        zoe_server::serve::TlsOptions::new(certs.join("server.pem"), certs.join("server.key"))
            .client_ca(certs.join("client-ca.pem"));
    let store = fresh_dir(name).join("store");
    Server::start(ServeOptions::new(store).tls(tls)).unwrap()
}

/// Open a client of `server` with the certificate `client`, the CA file
/// `authenticate`, the name check `check` and the server name `name`.
async fn open(
    server: &Server,
    authenticate: &str,
    check: bool,
    name: &str,
) -> Result<Client, Error> {
    let certs = certs();
    let options = TlsOptions::new(
        certs.join("client.pem"),
        certs.join(format!("{authenticate}.pem")),
    )
    .key(certs.join("client.key"))
    .check_hostname(check);
    open_with(server.addr(), options, name).await
}

/// Open a client of `addr` with the TLS of `options` and the server name
/// `name`.
async fn open_with(addr: SocketAddr, options: TlsOptions, name: &str) -> Result<Client, Error> {
    let tls = Tls::load(&options).unwrap();
    let endpoint = Endpoint::new(addr).tls(tls, name).unwrap();
    Client::open_endpoint(Arc::new(Tokio), &endpoint, "1", &Options::default()).await
}

#[tokio::test]
async fn the_client_opens_when_the_server_and_its_name_check() {
    let server = serve("good");
    let client = open(&server, "server-ca", true, "localhost").await.unwrap();
    assert!(client.is_connected());
    client.close().await;
}

#[tokio::test]
async fn a_wrong_name_fails_the_open_only_with_the_name_check() {
    let server = serve("name");
    let refused = open(&server, "server-ca", true, "example.org").await;
    assert!(matches!(refused, Err(Error::Io(_))), "{refused:?}");
    // Without the name check, the chain of the server is enough.
    let client = open(&server, "server-ca", false, "example.org")
        .await
        .unwrap();
    client.close().await;
}

#[tokio::test]
async fn without_the_name_check_a_ca_that_does_not_sign_the_server_fails_the_open() {
    let server = serve("chain");
    for name in ["localhost", "example.org"] {
        let refused = open(&server, "other-ca", false, name).await;
        assert!(matches!(refused, Err(Error::Io(_))), "{name}: {refused:?}");
        let refused = open(&server, "other-ca", true, name).await;
        assert!(matches!(refused, Err(Error::Io(_))), "{name}: {refused:?}");
    }
}

#[tokio::test]
async fn a_tls_client_never_speaks_to_a_server_without_tls() {
    let server = Server::start(ServeOptions::new(fresh_dir("plain").join("store"))).unwrap();
    let opened = open(&server, "server-ca", false, "localhost").await;
    assert!(opened.is_err(), "{opened:?}");
}

#[tokio::test]
async fn a_client_without_a_certificate_opens_when_the_server_asks_for_none() {
    let certs = certs();
    let tls =
        zoe_server::serve::TlsOptions::new(certs.join("server.pem"), certs.join("server.key"));
    let store = fresh_dir("anonymous").join("store");
    let server = Server::start(ServeOptions::new(store).tls(tls)).unwrap();
    let authenticate = certs.join("server-ca.pem");
    let options = TlsOptions::without_certificate(Some(&authenticate));
    let client = open_with(server.addr(), options, "localhost")
        .await
        .unwrap();
    assert!(client.is_connected());
    client.close().await;
    // A wrong name still fails the open.
    let options = TlsOptions::without_certificate(Some(&authenticate));
    let refused = open_with(server.addr(), options, "example.org").await;
    assert!(matches!(refused, Err(Error::Io(_))), "{refused:?}");
}

#[tokio::test]
async fn a_client_without_a_certificate_fails_the_open_when_the_server_asks_for_one() {
    let server = serve("anonymous-refused");
    let authenticate = certs().join("server-ca.pem");
    let options = TlsOptions::without_certificate(Some(&authenticate));
    let refused = open_with(server.addr(), options, "localhost").await;
    assert!(refused.is_err(), "{refused:?}");
}

/// Return a new directory `name` with a copy of the CA certificate of each
/// of `cas`.
fn ca_dir(name: &str, cas: &[&str]) -> PathBuf {
    let dir = fresh_dir(name);
    for ca in cas {
        let file = format!("{ca}.pem");
        std::fs::copy(certs().join(&file), dir.join(&file)).unwrap();
    }
    dir
}

#[tokio::test]
async fn a_directory_of_ca_files_as_authenticate_checks_the_chain() {
    let server = serve("dir");
    let (cert, key) = (certs().join("client.pem"), certs().join("client.key"));
    let dir = ca_dir("dir-good", &["other-ca", "server-ca"]);
    let options = TlsOptions::new(&cert, &dir).key(&key);
    let client = open_with(server.addr(), options, "localhost")
        .await
        .unwrap();
    client.close().await;
    let dir = ca_dir("dir-other", &["other-ca"]);
    for check in [true, false] {
        let options = TlsOptions::new(&cert, &dir).key(&key).check_hostname(check);
        let refused = open_with(server.addr(), options, "localhost").await;
        assert!(matches!(refused, Err(Error::Io(_))), "{check}: {refused:?}");
    }
}

/// The variable that starts the open of [`child_opens_with_the_system_store`]:
/// the address of the server, `open` or `refused`, and the directory of
/// the certificates.
const CHILD: &str = "ZOE_CLIENT_TLS_TEST_CHILD";

/// The line that the child prints when it ran.
const CHILD_RAN: &str = "the child opened with the system store";

/// Run [`child_opens_with_the_system_store`] in a new process of this test
/// binary, with `SSL_CERT_FILE` set to the CA certificate `ca` and without
/// `SSL_CERT_DIR`. So the store of the system holds `ca` only. The child
/// expects `outcome`. Fail when the child fails or does not run.
fn in_child(name: &str, ca: &str, outcome: &str) {
    let server = serve(name);
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_opens_with_the_system_store",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove("SSL_CERT_DIR")
        .env("SSL_CERT_FILE", certs().join(format!("{ca}.pem")))
        .env(
            CHILD,
            format!("{} {outcome} {}", server.addr(), certs().display()),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains(CHILD_RAN), "{stdout}\n{stderr}");
}

/// Open a client without `authenticate` when [`CHILD`] is set, else do
/// nothing. [`in_child`] runs it with the system store that it sets.
#[tokio::test]
async fn child_opens_with_the_system_store() {
    let Some(spec) = std::env::var_os(CHILD) else {
        return;
    };
    let spec = spec.into_string().unwrap();
    let mut parts = spec.splitn(3, ' ');
    let (addr, outcome) = (parts.next().unwrap(), parts.next().unwrap());
    let certs = Path::new(parts.next().unwrap());
    let options = TlsOptions::system(certs.join("client.pem")).key(certs.join("client.key"));
    let opened = open_with(addr.parse().unwrap(), options, "localhost").await;
    match outcome {
        "open" => opened.unwrap().close().await,
        _ => assert!(matches!(opened, Err(Error::Io(_))), "{opened:?}"),
    }
    println!("{CHILD_RAN}");
}

#[test]
fn without_authenticate_a_ca_of_the_system_store_checks_the_chain() {
    in_child("system", "server-ca", "open");
}

#[test]
fn without_authenticate_a_ca_that_the_system_store_does_not_hold_fails_the_open() {
    in_child("system-other", "other-ca", "refused");
}
