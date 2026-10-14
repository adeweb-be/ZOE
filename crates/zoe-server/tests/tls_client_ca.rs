//! The verifier of the client certificates of TLS on the ZEO port.
//!
//! With a client CA file, the server accepts a client certificate that is
//! byte for byte one of the certificates of the file, within its validity
//! period, also when it is a CA certificate. Its holder registers
//! read-write, also when the server has a read-only client CA. The client
//! still proves its key in the handshake. Each other client certificate
//! goes through the check of rustls: a CA certificate that is in no file,
//! that chains to the file, or that is in the read-only client CA file is
//! refused. A refused handshake closes only its connection.
//!
//! The test makes its certificates with the `openssl` command, so the
//! repository holds no key.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use zoe_server::serve::{ServeOptions, Server, TlsOptions};

mod wire;

use wire::{PROTOCOL, TIMEOUT, Value, fresh_dir, spawn_lock};

/// The extensions of a self-signed CA certificate.
const CA: &[&str] = &[
    "-addext",
    "basicConstraints = critical, CA:TRUE",
    "-addext",
    "keyUsage = critical, keyCertSign, cRLSign, digitalSignature",
];

/// Run `openssl` with `args` in `dir`. Fail the test when it fails.
fn openssl(dir: &Path, args: &[&str]) {
    let mut command = Command::new("openssl");
    command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = {
        let _spawn = spawn_lock();
        command.output().unwrap()
    };
    assert!(
        output.status.success(),
        "openssl {args:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Make a self-signed CA certificate `NAME.pem` with its key `NAME.key`.
fn self_signed(dir: &Path, name: &str, period: &[&str]) {
    let subject = format!("/CN={name}");
    let key = format!("{name}.key");
    let cert = format!("{name}.pem");
    let mut args = vec![
        "req",
        "-x509",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:prime256v1",
        "-nodes",
        "-subj",
        &subject,
        "-keyout",
        &key,
        "-out",
        &cert,
    ];
    args.extend_from_slice(CA);
    args.extend_from_slice(period);
    openssl(dir, &args);
}

/// Make the certificate `NAME.pem` with the extensions `extensions` and
/// its key `NAME.key`, signed by the key of the CA `ca`.
fn signed(dir: &Path, name: &str, ca: &str, extensions: &str) {
    let subject = format!("/CN={name}");
    let key = format!("{name}.key");
    let request = format!("{name}.csr");
    let ext = format!("{name}.ext");
    let cert = format!("{name}.pem");
    let ca_cert = format!("{ca}.pem");
    let ca_key = format!("{ca}.key");
    let serial = format!("{ca}.srl");
    openssl(
        dir,
        &[
            "req",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-subj",
            &subject,
            "-keyout",
            &key,
            "-out",
            &request,
        ],
    );
    std::fs::write(dir.join(&ext), extensions).unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            &request,
            "-CA",
            &ca_cert,
            "-CAkey",
            &ca_key,
            "-CAserial",
            &serial,
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            &ext,
            "-out",
            &cert,
        ],
    );
}

/// Return the directory of the certificates of the test. The first call
/// makes them, before any server of the test starts.
///
/// - `server.pem`: the certificate of the server, signed by `server-ca`.
/// - `client-ca`, `other-ca`, `read-only-ca`: self-signed CA certificates.
/// - `sub-ca`: a CA certificate signed by the key of `client-ca`.
/// - `leaf`: a client certificate, not a CA, signed by the same key.
/// - `expired-ca`, `future-ca`: self-signed CA certificates out of their
///   validity periods.
/// - `client-cas.pem`: `client-ca`, `expired-ca` and `future-ca`.
fn certs() -> &'static Path {
    static CERTS: OnceLock<PathBuf> = OnceLock::new();
    CERTS.get_or_init(|| {
        let dir = fresh_dir("tls-client-ca-certs");
        let days = &["-days", "2"][..];
        for name in ["server-ca", "client-ca", "other-ca", "read-only-ca"] {
            self_signed(&dir, name, days);
        }
        self_signed(
            &dir,
            "expired-ca",
            &[
                "-not_before",
                "20000101000000Z",
                "-not_after",
                "20010101000000Z",
            ],
        );
        self_signed(
            &dir,
            "future-ca",
            &[
                "-not_before",
                "21000101000000Z",
                "-not_after",
                "21010101000000Z",
            ],
        );
        signed(
            &dir,
            "server",
            "server-ca",
            "basicConstraints = critical, CA:FALSE\n\
             extendedKeyUsage = serverAuth\n\
             subjectAltName = IP:127.0.0.1, IP:::1\n",
        );
        signed(
            &dir,
            "sub-ca",
            "client-ca",
            "basicConstraints = critical, CA:TRUE\n\
             keyUsage = critical, keyCertSign, cRLSign, digitalSignature\n",
        );
        signed(
            &dir,
            "leaf",
            "client-ca",
            "basicConstraints = critical, CA:FALSE\n\
             extendedKeyUsage = clientAuth\n",
        );
        let mut cas = Vec::new();
        for name in ["client-ca", "expired-ca", "future-ca"] {
            cas.extend(std::fs::read(dir.join(format!("{name}.pem"))).unwrap());
        }
        std::fs::write(dir.join("client-cas.pem"), cas).unwrap();
        dir
    })
}

/// Start a server for the test `name` with the client CA file
/// `client_ca` and the read-only client CA file `read_only_client_ca`.
fn serve(name: &str, client_ca: Option<&str>, read_only_client_ca: Option<&str>) -> Server {
    let certs = certs();
    let mut tls = TlsOptions::new(certs.join("server.pem"), certs.join("server.key"));
    if let Some(ca) = client_ca {
        tls = tls.client_ca(certs.join(ca));
    }
    if let Some(ca) = read_only_client_ca {
        tls = tls.read_only_client_ca(certs.join(ca));
    }
    let store = fresh_dir(name).join("store");
    Server::start(ServeOptions::new(store).tls(tls)).unwrap()
}

/// A client of the ZEO port over TLS.
struct Client {
    stream: StreamOwned<ClientConnection, TcpStream>,
}

impl Client {
    /// Connect to `addr` over TLS with the certificate `CERT.pem` and the
    /// key `KEY.key`, then read the protocol of the server and answer it.
    /// The key does not have to be the key of the certificate. Fail when
    /// the server refuses the client.
    fn connect(addr: SocketAddr, cert: &str, key: &str) -> io::Result<Client> {
        let certs = certs();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(certs.join("server-ca.pem")).unwrap())
            .unwrap();
        let chain = vec![CertificateDer::from_pem_file(certs.join(format!("{cert}.pem"))).unwrap()];
        let key = PrivateKeyDer::from_pem_file(certs.join(format!("{key}.key"))).unwrap();
        let key = provider.key_provider.load_private_key(key).unwrap();
        // `CertifiedKey::new` does not check that the key fits the
        // certificate.
        let resolver = SingleCertAndKey::from(CertifiedKey::new(chain, key));
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_client_cert_resolver(Arc::new(resolver));
        let connection =
            ClientConnection::new(Arc::new(config), ServerName::from(addr.ip())).unwrap();
        let tcp = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        tcp.set_read_timeout(Some(TIMEOUT))?;
        let mut client = Client {
            stream: StreamOwned::new(connection, tcp),
        };
        // With TLS 1.3, the server checks the client certificate after the
        // client ends its handshake. A refused client then reads an alert
        // or the end of the stream, not the protocol.
        let protocol = client.read_frame()?;
        assert_eq!(protocol, PROTOCOL);
        client.write_frame(PROTOCOL)?;
        Ok(client)
    }

    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        let length = u32::try_from(bytes.len()).unwrap();
        self.stream.write_all(&length.to_be_bytes())?;
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }

    fn read_frame(&mut self) -> io::Result<Vec<u8>> {
        let mut length = [0u8; 4];
        self.stream.read_exact(&mut length)?;
        let mut bytes = vec![0u8; u32::from_be_bytes(length) as usize];
        self.stream.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Register with the storage `1` to write. Return `Ok` when the server
    /// accepts it, else the class of the error.
    fn register_to_write(&mut self) -> Result<(), String> {
        let call = Value::Array(vec![
            Value::from(1),
            Value::Boolean(false),
            Value::from("register"),
            Value::Array(vec![Value::from("1"), Value::Boolean(false)]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &call).unwrap();
        self.write_frame(&bytes).unwrap();
        loop {
            let frame = self.read_frame().unwrap();
            let Value::Array(parts) = rmpv::decode::read_value(&mut frame.as_slice()).unwrap()
            else {
                panic!("a message is not an array");
            };
            if parts[2].as_str() != Some(".reply") || parts[0] != Value::from(1) {
                continue;
            }
            if parts[1] != Value::from(2) {
                return Ok(());
            }
            return Err(format!("{}", parts[3]));
        }
    }
}

/// Check that the server refuses the client with the certificate
/// `CERT.pem` and the key `KEY.key`.
fn assert_refused(addr: SocketAddr, cert: &str, key: &str) {
    assert!(
        Client::connect(addr, cert, key).is_err(),
        "the server accepts {cert}.pem with {key}.key"
    );
}

#[test]
fn a_ca_certificate_of_the_client_ca_file_registers_read_write() {
    let server = serve("tls-pinned", Some("client-ca.pem"), None);
    let mut client = Client::connect(server.addr(), "client-ca", "client-ca").unwrap();
    assert_eq!(client.register_to_write(), Ok(()));
    drop(client);
    server.stop();
}

#[test]
fn a_ca_certificate_of_the_client_ca_file_registers_read_write_with_a_read_only_ca() {
    let server = serve(
        "tls-pinned-read-only",
        Some("client-ca.pem"),
        Some("read-only-ca.pem"),
    );
    let mut client = Client::connect(server.addr(), "client-ca", "client-ca").unwrap();
    assert_eq!(client.register_to_write(), Ok(()));
    drop(client);
    // A CA certificate of the read-only client CA file does not match by
    // its bytes.
    assert_refused(server.addr(), "read-only-ca", "read-only-ca");
    server.stop();
}

#[test]
fn a_ca_certificate_of_the_read_only_client_ca_file_is_refused() {
    let server = serve("tls-read-only-ca", None, Some("read-only-ca.pem"));
    assert_refused(server.addr(), "read-only-ca", "read-only-ca");
    let server_2 = serve(
        "tls-read-only-ca-2",
        Some("other-ca.pem"),
        Some("client-ca.pem"),
    );
    assert_refused(server_2.addr(), "client-ca", "client-ca");
    server.stop();
    server_2.stop();
}

#[test]
fn a_client_without_the_key_of_the_certificate_is_refused() {
    let server = serve("tls-pinned-other-key", Some("client-ca.pem"), None);
    assert_refused(server.addr(), "client-ca", "other-ca");
    // The server goes on.
    let client = Client::connect(server.addr(), "client-ca", "client-ca").unwrap();
    drop(client);
    server.stop();
}

#[test]
fn a_ca_certificate_that_is_in_no_file_or_chains_to_the_file_is_refused() {
    let server = serve("tls-other-cas", Some("client-ca.pem"), None);
    assert_refused(server.addr(), "other-ca", "other-ca");
    assert_refused(server.addr(), "sub-ca", "sub-ca");
    // A certificate that is not a CA and that chains to the file is
    // accepted.
    let mut client = Client::connect(server.addr(), "leaf", "leaf").unwrap();
    assert_eq!(client.register_to_write(), Ok(()));
    drop(client);
    server.stop();
}

#[test]
fn a_certificate_of_the_file_out_of_its_validity_period_is_refused() {
    let server = serve("tls-pinned-period", Some("client-cas.pem"), None);
    assert_refused(server.addr(), "expired-ca", "expired-ca");
    assert_refused(server.addr(), "future-ca", "future-ca");
    // The other certificate of the same file is accepted.
    let mut client = Client::connect(server.addr(), "client-ca", "client-ca").unwrap();
    assert_eq!(client.register_to_write(), Ok(()));
    drop(client);
    server.stop();
}
