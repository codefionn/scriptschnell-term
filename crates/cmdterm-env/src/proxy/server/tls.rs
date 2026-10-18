//! TLS interception for tunnels to allowed hosts.
//!
//! The proxy ends the sandbox's TLS session itself and opens its own to the
//! host. It shows the sandbox a certificate for the host signed by one of
//! two certificate authorities that exist only in memory for this run:
//!
//! - the trusted CA, when the host's certificate verifies against this
//!   machine's roots. Sandboxes trust it through the bundle file
//!   ([`Interceptor::bundle`]), which holds the system roots plus this CA.
//! - the untrusted CA, when it does not (self-signed, expired, wrong name).
//!   Nothing trusts it, so the client inside still sees a certificate it
//!   cannot verify and must opt in (`curl -k`), as it would without the
//!   proxy. A bad upstream certificate never turns into a trusted one.
//!
//! ALPN follows the host: the sandbox is offered what the host picked from
//! the protocols the sandbox asked for, so HTTP/2 passes through. Tunnels
//! with a [`RequestFilter`] offer HTTP/1.1 only, the version it reads.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::Acceptor;
use rustls::{
    ClientConfig, ClientConnection, Connection, DigitallySignedStruct, RootCertStore, ServerConfig,
    SignatureScheme,
};

use super::Duplex;
use super::http::RequestFilter;

/// Leaf certificates kept per host before the cache starts over.
const MAX_LEAVES: usize = 1024;

type Leaf = (Vec<CertificateDer<'static>>, Arc<PrivateKeyDer<'static>>);

pub(super) struct Interceptor {
    provider: Arc<CryptoProvider>,
    roots: Arc<RootCertStore>,
    trusted: CertifiedIssuer<'static, KeyPair>,
    untrusted: CertifiedIssuer<'static, KeyPair>,
    bundle: PathBuf,
    /// Per `(host, trusted)`.
    leaves: Mutex<HashMap<(String, bool), Leaf>>,
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn ca(name: &str) -> io::Result<CertifiedIssuer<'static, KeyPair>> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    dn.push(DnType::OrganizationName, "cmdterm");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(365);
    let key = KeyPair::generate().map_err(other)?;
    CertifiedIssuer::self_signed(params, key).map_err(other)
}

impl Interceptor {
    /// New CAs for this run, and the bundle sandboxes trust written to
    /// `bundle`: this machine's roots, then the trusted CA.
    pub(super) fn new(bundle: &Path) -> io::Result<Self> {
        // The CAs the user's tools trust through variables such as
        // NODE_EXTRA_CA_CERTS, so a company's TLS inspection works through
        // the proxy as it does on the host.
        let mut roots = rustls_native_certs::load_native_certs().certs;
        roots.extend(
            crate::host_env::ca_certificates()
                .into_iter()
                .map(CertificateDer::from),
        );
        Self::with_roots(bundle, roots)
    }

    /// Like [`Self::new`], with `native` in place of this machine's roots.
    pub(super) fn with_roots(
        bundle: &Path,
        mut native: Vec<CertificateDer<'static>>,
    ) -> io::Result<Self> {
        let mut seen = std::collections::HashSet::new();
        native.retain(|cert| seen.insert(cert.to_vec()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(native.iter().cloned());
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let trusted = ca("cmdterm sandbox proxy")?;
        let untrusted = ca("cmdterm sandbox proxy (untrusted upstream)")?;
        let mut pem = String::new();
        for cert in &native {
            pem.push_str(&pem::encode(&pem::Pem::new("CERTIFICATE", cert.to_vec())));
        }
        pem.push_str(&trusted.pem());
        std::fs::write(bundle, pem)?;
        Ok(Self {
            provider,
            roots: Arc::new(roots),
            trusted,
            untrusted,
            bundle: bundle.to_path_buf(),
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn bundle(&self) -> &Path {
        &self.bundle
    }

    fn leaf(&self, host: &str, trusted: bool) -> io::Result<Leaf> {
        let key = (host.to_owned(), trusted);
        if let Some(leaf) = self.leaves.lock().map_err(other)?.get(&key) {
            return Ok(leaf.clone());
        }
        let mut params = CertificateParams::new(vec![host.to_owned()]).map_err(other)?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(30);
        let issuer = if trusted {
            &self.trusted
        } else {
            &self.untrusted
        };
        let key_pair = KeyPair::generate().map_err(other)?;
        let cert = params.signed_by(&key_pair, issuer).map_err(other)?;
        let private = PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());
        let leaf = (
            vec![cert.der().clone(), issuer.der().clone()],
            Arc::new(private),
        );
        let mut leaves = self.leaves.lock().map_err(other)?;
        if leaves.len() >= MAX_LEAVES {
            leaves.clear();
        }
        leaves.insert(key, leaf.clone());
        Ok(leaf)
    }

    /// A TLS session to `host` over `tcp`, checking its certificate when
    /// `verify`.
    fn upstream(
        &self,
        mut tcp: TcpStream,
        host: &str,
        alpn: &[Vec<u8>],
        verify: bool,
    ) -> io::Result<(ClientConnection, TcpStream)> {
        let builder = ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .map_err(other)?;
        let mut config = if verify {
            builder
                .with_root_certificates(Arc::clone(&self.roots))
                .with_no_client_auth()
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCert(
                    self.provider.signature_verification_algorithms,
                )))
                .with_no_client_auth()
        };
        config.alpn_protocols = alpn.to_vec();
        let name = ServerName::try_from(host.to_owned()).map_err(other)?;
        let mut conn = ClientConnection::new(Arc::new(config), name).map_err(other)?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        while conn.is_handshaking() {
            conn.complete_io(&mut tcp)?;
        }
        tcp.set_read_timeout(None)?;
        Ok((conn, tcp))
    }

    /// Serves one tunnel to `host` after the proxy answered `CONNECT`.
    /// `early` holds the bytes the client sent after its `CONNECT` head, and
    /// `tcp` is the connection to the host. `filter` reads what the client
    /// sends before it goes to the host. Returns
    /// [`Served::Distrusted`] when the client turned down a certificate of
    /// the trusted CA.
    pub(super) fn serve<S: Duplex>(
        &self,
        mut client: S,
        early: &[u8],
        host: &str,
        tcp: TcpStream,
        filter: Option<RequestFilter>,
    ) -> Served {
        client.set_timeout(Some(HANDSHAKE_TIMEOUT));
        let mut acceptor = Acceptor::default();
        let mut early = early;
        while !early.is_empty() {
            if acceptor.read_tls(&mut early).is_err() {
                return Served::Done;
            }
        }
        let accepted = loop {
            match acceptor.accept() {
                Ok(Some(accepted)) => break accepted,
                Ok(None) => match acceptor.read_tls(&mut client) {
                    Ok(0) | Err(_) => return Served::Done,
                    Ok(_) => {}
                },
                Err((_, mut alert)) => {
                    let _ = alert.write_all(&mut client);
                    return Served::Done;
                }
            }
        };
        let alpn: Vec<Vec<u8>> = accepted
            .client_hello()
            .alpn()
            .map(|list| list.map(<[u8]>::to_vec).collect())
            .unwrap_or_default();
        let alpn = if filter.is_some() && !alpn.is_empty() {
            vec![b"http/1.1".to_vec()]
        } else {
            alpn
        };
        // The second try goes to the address the first one reached, which
        // the proxy already checked, not to whatever the name resolves to
        // now.
        let peer = tcp.peer_addr();
        let (upstream, tcp, trusted) = match self.upstream(tcp, host, &alpn, true) {
            Ok((conn, tcp)) => (conn, tcp, true),
            Err(error) if is_certificate_error(&error) => {
                let retry =
                    peer.and_then(|peer| TcpStream::connect_timeout(&peer, super::CONNECT_TIMEOUT));
                match retry.and_then(|tcp| self.upstream(tcp, host, &alpn, false)) {
                    Ok((conn, tcp)) => (conn, tcp, false),
                    Err(_) => return Served::Done,
                }
            }
            Err(_) => return Served::Done,
        };
        let Ok((chain, key)) = self.leaf(host, trusted) else {
            return Served::Done;
        };
        // A client may still go on past the untrusted certificate, but the
        // upstream it reaches could be anyone, so it gets no real secret.
        let filter = if trusted {
            filter
        } else {
            filter.map(RequestFilter::without_secrets)
        };
        let Ok(builder) = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
        else {
            return Served::Done;
        };
        let Ok(mut config) = builder
            .with_no_client_auth()
            .with_single_cert(chain, key.clone_key())
        else {
            return Served::Done;
        };
        config.alpn_protocols = upstream
            .alpn_protocol()
            .map(|protocol| vec![protocol.to_vec()])
            .unwrap_or_default();
        let mut server = match accepted.into_connection(Arc::new(config)) {
            Ok(server) => server,
            Err((_, mut alert)) => {
                let _ = alert.write_all(&mut client);
                return Served::Done;
            }
        };
        while server.is_handshaking() {
            if let Err(error) = server.complete_io(&mut client) {
                // A client always turns down the untrusted CA's
                // certificates; only the trusted one's say something.
                return if trusted && is_distrust_alert(&error) {
                    Served::Distrusted
                } else {
                    Served::Done
                };
            }
        }
        client.set_timeout(None);
        relay(
            client,
            Connection::Server(server),
            tcp,
            Connection::Client(upstream),
            filter,
        );
        Served::Done
    }
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How a tunnel through [`Interceptor::serve`] ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Served {
    Done,
    /// The client ended the handshake with an alert that says it does not
    /// trust the certificate: it ignores the proxy's CA bundle.
    Distrusted,
}

/// True when the client's alert turned down the certificate it was shown.
fn is_distrust_alert(error: &io::Error) -> bool {
    use rustls::AlertDescription::{BadCertificate, CertificateUnknown, UnknownCA};
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|error| {
            matches!(
                error,
                rustls::Error::AlertReceived(UnknownCA | BadCertificate | CertificateUnknown)
            )
        })
}

/// True when a handshake failed over the peer's certificate.
fn is_certificate_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|error| {
            matches!(
                error,
                rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented
            )
        })
}

/// Accepts any certificate, but still checks the handshake signatures, so
/// the session is at least with whoever holds the certificate's key.
#[derive(Debug)]
pub(super) struct AnyCert(pub(super) WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// Writes what `conn` has queued for its peer.
fn flush(conn: &mut Connection, out: &mut impl Write) -> io::Result<()> {
    while conn.wants_write() {
        conn.write_tls(out)?;
    }
    Ok(())
}

/// Copies plaintext both ways between two TLS sessions until both ends
/// closed, through `filter` from `a` to `b`. Each session sits behind a
/// lock that is never held across a blocking socket read.
fn relay<A: Duplex, B: Duplex>(
    a: A,
    a_conn: Connection,
    b: B,
    b_conn: Connection,
    filter: Option<RequestFilter>,
) {
    let a_conn = Arc::new(Mutex::new(a_conn));
    let b_conn = Arc::new(Mutex::new(b_conn));
    let (Ok(a_read), Ok(a_write), Ok(b_read), Ok(b_write)) =
        (a.split(), a.split(), b.split(), b.split())
    else {
        return;
    };
    let forward = {
        let (a_conn, b_conn) = (Arc::clone(&a_conn), Arc::clone(&b_conn));
        thread::spawn(move || pump(a_read, &a_conn, a_write, &b_conn, b, filter))
    };
    pump(b_read, &b_conn, b_write, &a_conn, a, None);
    let _ = forward.join();
}

/// Reads records from `from_sock` into `from`, and writes the plaintext
/// into `to`, through `filter` if there is one. Stops at the end of
/// `from`'s stream, on an error or when the filter refuses a request (its
/// response goes back to `from`), then closes `to` for writing.
fn pump<A: Duplex, B: Duplex>(
    mut from_sock: A,
    from: &Mutex<Connection>,
    mut from_out: A,
    to: &Mutex<Connection>,
    mut to_out: B,
    mut filter: Option<RequestFilter>,
) {
    let mut raw = [0u8; 16 * 1024];
    let mut plain = [0u8; 16 * 1024];
    let mut data = Vec::new();
    let mut filtered = Vec::new();
    // The handshake may already have read plaintext (a client's first
    // request often comes with its Finished message), so each round takes
    // what the session holds before it waits on the socket.
    let mut chunk: &[u8] = &[];
    'outer: loop {
        let mut closed = false;
        loop {
            let Ok(mut conn) = from.lock() else {
                break 'outer;
            };
            if !chunk.is_empty() {
                if conn.read_tls(&mut chunk).is_err() {
                    break 'outer;
                }
                let processed = conn.process_new_packets();
                let _ = flush(&mut conn, &mut from_out);
                if processed.is_err() {
                    break 'outer;
                }
            }
            loop {
                match conn.reader().read(&mut plain) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(m) => data.extend_from_slice(&plain[..m]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break 'outer,
                }
            }
            if chunk.is_empty() {
                break;
            }
        }
        if !data.is_empty() {
            let mut refused = None;
            if let Some(filter) = filter.as_mut() {
                filtered.clear();
                refused = filter.feed(&data, &mut filtered).err();
                std::mem::swap(&mut data, &mut filtered);
            }
            let Ok(mut conn) = to.lock() else {
                break;
            };
            if conn.writer().write_all(&data).is_err() || flush(&mut conn, &mut to_out).is_err() {
                break;
            }
            drop(conn);
            data.clear();
            if let Some(response) = refused {
                if let Ok(mut conn) = from.lock() {
                    let _ = conn.writer().write_all(&response);
                    conn.send_close_notify();
                    let _ = flush(&mut conn, &mut from_out);
                }
                from_out.close_write();
                super::linger(&mut from_sock);
                break;
            }
        }
        if closed {
            break;
        }
        let n = match from_sock.read(&mut raw) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        chunk = &raw[..n];
    }
    if let Ok(mut conn) = to.lock() {
        conn.send_close_notify();
        let _ = flush(&mut conn, &mut to_out);
    }
    to_out.close_write();
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use rustls::{ServerConnection, StreamOwned};

    use std::sync::atomic::Ordering;

    use super::*;
    use crate::proxy::SandboxProxy;
    use crate::proxy::rules::{GuardMode, GuardRule, ProxyRules, SecretRule};
    use crate::proxy::server::NetworkProxy;

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// A TLS server on loopback for `localhost` that answers each read
    /// with `echo: ` and what it read. With a CA its certificate is signed
    /// by it, else it is self-signed.
    fn echo_server(ca: Option<&CertifiedIssuer<'static, KeyPair>>) -> u16 {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().unwrap();
        let cert = match ca {
            Some(ca) => params.signed_by(&key, ca).unwrap(),
            None => params.self_signed(&key).unwrap(),
        };
        let config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let config = Arc::clone(&config);
                thread::spawn(move || {
                    let conn = ServerConnection::new(config).unwrap();
                    let mut tls = StreamOwned::new(conn, tcp);
                    let mut buf = [0u8; 64];
                    while let Ok(n @ 1..) = tls.read(&mut buf) {
                        let mut reply = b"echo: ".to_vec();
                        reply.extend_from_slice(&buf[..n]);
                        if tls.write_all(&reply).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    /// Opens a tunnel to `localhost:port` through the proxy and speaks TLS
    /// in it with `config`. Returns the reply to `ping` and the chain the
    /// proxy showed.
    fn ask(
        proxy: &SandboxProxy,
        port: u16,
        config: ClientConfig,
    ) -> io::Result<(String, Vec<CertificateDer<'static>>)> {
        say(proxy, port, config, b"ping")
    }

    /// [`ask`] with `message` in place of `ping`.
    fn say(
        proxy: &SandboxProxy,
        port: u16,
        config: ClientConfig,
        message: &[u8],
    ) -> io::Result<(String, Vec<CertificateDer<'static>>)> {
        let mut tcp = TcpStream::connect(("127.0.0.1", proxy.port))?;
        let auth = proxy.token.as_ref().unwrap().header();
        write!(
            tcp,
            "CONNECT localhost:{port} HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"
        )?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            tcp.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 200"));
        let name = ServerName::try_from("localhost").unwrap();
        let conn = ClientConnection::new(Arc::new(config), name).map_err(other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(message)?;
        let mut buf = [0u8; 512];
        let n = tls.read(&mut buf)?;
        let chain = tls
            .conn
            .peer_certificates()
            .unwrap_or_default()
            .iter()
            .map(|c| c.clone().into_owned())
            .collect();
        Ok((String::from_utf8_lossy(&buf[..n]).into_owned(), chain))
    }

    fn trusting(certs: &[CertificateDer<'static>]) -> ClientConfig {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(certs.iter().cloned());
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    fn trusting_anything() -> ClientConfig {
        let provider = provider();
        let algorithms = provider.signature_verification_algorithms;
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(algorithms)))
            .with_no_client_auth()
    }

    #[test]
    fn valid_hosts_get_the_trusted_ca_and_invalid_ones_the_untrusted_one() {
        let dir = tempfile::tempdir().unwrap();
        let upstream_ca = ca("upstream test CA").unwrap();
        let good = echo_server(Some(&upstream_ca));
        let bad = echo_server(None);
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        let bundle = dir.path().join("bundle.pem");
        // A root named twice, such as by the platform store and by
        // NODE_EXTRA_CA_CERTS, lands in the bundle once.
        let root = upstream_ca.der().clone();
        let interceptor = Interceptor::with_roots(&bundle, vec![root.clone(), root]).unwrap();
        *proxy.shared.tls.write().unwrap() = Some(Arc::new(interceptor));
        proxy.shared.intercept_all.store(true, Ordering::Relaxed);
        assert_eq!(
            proxy.endpoint().ca_bundle.as_deref(),
            Some(bundle.as_path())
        );
        let port = &proxy.endpoint();

        // The bundle holds the roots and then the proxy's CA.
        let pems = pem::parse_many(std::fs::read(&bundle).unwrap()).unwrap();
        assert_eq!(pems.len(), 2);
        let proxy_ca = CertificateDer::from(pems[1].contents().to_vec());

        // Only the bundle's CA is needed to reach a valid host.
        let (reply, chain) = ask(port, good, trusting(std::slice::from_ref(&proxy_ca))).unwrap();
        assert_eq!(reply, "echo: ping");
        assert_eq!(chain[1], proxy_ca);

        // A self-signed host stays untrusted behind the proxy.
        let error = ask(port, bad, trusting(std::slice::from_ref(&proxy_ca))).unwrap_err();
        assert!(is_certificate_error(&error), "{error}");
        let (reply, chain) = ask(port, bad, trusting_anything()).unwrap();
        assert_eq!(reply, "echo: ping");
        assert_ne!(chain[1], proxy_ca);
        // Turning down the untrusted CA is expected and not logged.
        assert!(proxy.refusals().list().is_empty());

        // A client that ignores the bundle turns down the trusted CA, and
        // the refusal log says so.
        let upstream_only = trusting(std::slice::from_ref(upstream_ca.der()));
        assert!(ask(port, good, upstream_only).is_err());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let refusals = loop {
            let refusals = proxy.refusals().list();
            if !refusals.is_empty() || std::time::Instant::now() > deadline {
                break refusals;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].host, "localhost");
        assert!(refusals[0].reason.contains("CA bundle"), "{refusals:?}");

        proxy.intercept_tls(None, false).unwrap();
        assert!(!bundle.exists());
        assert_eq!(proxy.endpoint().ca_bundle, None);
    }

    #[test]
    fn inspected_hosts_get_real_secrets_and_their_guards() {
        let dir = tempfile::tempdir().unwrap();
        let upstream_ca = ca("upstream test CA").unwrap();
        let server = echo_server(Some(&upstream_ca));
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        let port = &proxy.endpoint();
        proxy.set_rules(ProxyRules {
            secrets: vec![SecretRule {
                fake: "npm_FAKE".into(),
                real: "npm_REAL".into(),
                hosts: vec!["localhost".into()],
            }],
            guards: vec![GuardRule {
                id: "npm.publish".into(),
                title: "npm publish".into(),
                hosts: vec!["localhost".into()],
                requests: vec!["PUT /*".into()],
                mode: GuardMode::Block,
                kind: None,
            }],
        });
        // Inspected hosts need interception.
        let mut tcp = TcpStream::connect(("127.0.0.1", port.port)).unwrap();
        let auth = port.token.as_ref().unwrap().header();
        write!(
            tcp,
            "CONNECT localhost:{server} HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        let _ = tcp.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains("needs HTTPS inspection"), "{reply}");

        let bundle = dir.path().join("bundle.pem");
        let interceptor =
            Interceptor::with_roots(&bundle, vec![upstream_ca.der().clone()]).unwrap();
        *proxy.shared.tls.write().unwrap() = Some(Arc::new(interceptor));
        let pems = pem::parse_many(std::fs::read(&bundle).unwrap()).unwrap();
        let proxy_ca = CertificateDer::from(pems[1].contents().to_vec());
        let mut config = trusting(std::slice::from_ref(&proxy_ca));
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let (reply, _) = say(
            port,
            server,
            config.clone(),
            b"GET /pkg HTTP/1.1\r\nAuthorization: Bearer npm_FAKE\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            reply,
            "echo: GET /pkg HTTP/1.1\r\nAuthorization: Bearer npm_REAL\r\n\r\n"
        );
        let (reply, _) = say(port, server, config, b"PUT /pkg HTTP/1.1\r\n\r\n").unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains("npm publish is blocked"), "{reply}");

        // An upstream whose certificate fails verification gets the guards
        // but not the real secret, even when the client goes on.
        let untrusted = echo_server(None);
        let get = b"GET /pkg HTTP/1.1\r\nAuthorization: Bearer npm_FAKE\r\n\r\n";
        let (reply, _) = say(port, untrusted, trusting_anything(), get).unwrap();
        assert_eq!(
            reply,
            "echo: GET /pkg HTTP/1.1\r\nAuthorization: Bearer npm_FAKE\r\n\r\n"
        );
        let put = b"PUT /pkg HTTP/1.1\r\n\r\n";
        let (reply, _) = say(port, untrusted, trusting_anything(), put).unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    }
}
