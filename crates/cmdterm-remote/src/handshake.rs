//! The connection handshake.
//!
//! Three messages, then sealed traffic:
//!
//! 1. Client hello, plain: magic, an ephemeral X25519 key, an ephemeral
//!    ML-KEM-768 encapsulation key, and a nonce.
//! 2. Server hello, plain: the server's ephemeral X25519 key, an ML-KEM
//!    ciphertext for the client's key, a nonce, the server identity, and the
//!    server's hybrid signature over both hellos.
//! 3. Client auth, sealed: the client identity and its hybrid signature over
//!    the transcript hash. The server answers with a sealed accept or reject.
//!
//! Session keys come from SHA3-256 over both shared secrets and the
//! transcript hash, in the style of the X-Wing combiner: an attacker needs
//! both the X25519 and the ML-KEM secret. The client checks the server key
//! against `known_hosts` before it sends anything that names the client, so a
//! server it does not trust never learns who connected.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use ml_kem::kem::{Decapsulate, Encapsulate};
use ml_kem::{KeyExport, MlKem768};
use sha3::{Digest, Sha3_256};
use zeroize::Zeroizing;

use crate::keys::{Identity, KeyOptions, PUBLIC_LEN, PublicKey, SIGNATURE_LEN};
use crate::sys::UnixStream;
use crate::transport::{FrameWrite, Plain, SealKey, SealedReader, SealedWriter};

const MAGIC: &[u8; 16] = b"CMDTERM-REMOTE/1";
const X25519_LEN: usize = 32;
const KEM_EK_LEN: usize = 1184;
const KEM_CT_LEN: usize = 1088;
const NONCE_LEN: usize = 32;
const CLIENT_HELLO_LEN: usize = MAGIC.len() + X25519_LEN + KEM_EK_LEN + NONCE_LEN;
const SERVER_SIGNED_LEN: usize = X25519_LEN + KEM_CT_LEN + NONCE_LEN + PUBLIC_LEN;
const SERVER_HELLO_LEN: usize = SERVER_SIGNED_LEN + SIGNATURE_LEN;

/// Longest login answer a client reads: a flag byte, or a refusal with
/// its reason.
const MAX_ANSWER_LEN: usize = 1024;

const ACCEPT: u8 = 0;
const REJECT: u8 = 1;
const FLAG_VM_ONLY: u8 = 1;

/// A stream that can be split into a read half and a write half.
pub trait Duplex: Read + Write + Send + Sized + 'static {
    fn try_split(&self) -> io::Result<Self>;
}

impl Duplex for TcpStream {
    fn try_split(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

impl Duplex for UnixStream {
    fn try_split(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

/// An authenticated connection.
pub struct Session<S> {
    pub reader: SealedReader<S>,
    pub writer: SealedWriter<S>,
    /// The other side's identity.
    pub peer: PublicKey,
    /// What the server allows this client. Set on both sides.
    pub options: KeyOptions,
}

/// Runs the client side. `check_host` decides whether to trust the server
/// key; an `Err` ends the handshake before the client identifies itself.
pub fn client<S: Duplex>(
    stream: S,
    identity: &Identity,
    check_host: impl FnOnce(&PublicKey) -> io::Result<()>,
) -> io::Result<Session<S>> {
    let mut read = Plain(stream.try_split()?);
    let mut write = Plain(stream);

    let x_secret = x25519_secret()?;
    let x_public = x25519_dalek::PublicKey::from(&x_secret);
    let mut kem_seed = Zeroizing::new(ml_kem::Seed::default());
    getrandom::fill(kem_seed.as_mut_slice()).map_err(io::Error::other)?;
    let kem_dk = ml_kem::DecapsulationKey::<MlKem768>::from_seed(*kem_seed);
    let kem_ek = kem_dk.encapsulation_key().to_bytes();

    let mut hello = Vec::with_capacity(CLIENT_HELLO_LEN);
    hello.extend_from_slice(MAGIC);
    hello.extend_from_slice(x_public.as_bytes());
    hello.extend_from_slice(&kem_ek);
    hello.extend_from_slice(&random::<NONCE_LEN>()?);
    write.write_frame(&hello)?;

    let mut reply = Vec::new();
    if !read.read_frame_max(&mut reply, SERVER_HELLO_LEN)? || reply.len() != SERVER_HELLO_LEN {
        return Err(protocol("server hello has the wrong size"));
    }
    let (signed, signature) = reply.split_at(SERVER_SIGNED_LEN);
    let (server_x, rest) = signed.split_at(X25519_LEN);
    let (kem_ct, rest) = rest.split_at(KEM_CT_LEN);
    let server_key = PublicKey::from_bytes(&rest[NONCE_LEN..])
        .ok_or_else(|| protocol("server sent an invalid identity"))?;
    if !server_key.verify(&server_digest(&hello, signed), signature) {
        return Err(protocol("server signature does not verify"));
    }
    check_host(&server_key)?;

    let x_shared = x_secret.diffie_hellman(&x25519_public(server_x));
    if !x_shared.was_contributory() {
        return Err(protocol("server sent a weak X25519 key"));
    }
    let ct = ml_kem::Ciphertext::<MlKem768>::try_from(kem_ct)
        .map_err(|_| protocol("bad ML-KEM ciphertext"))?;
    let kem_shared = kem_dk.decapsulate(&ct);
    let transcript = transcript(&hello, &reply);
    let (c2s, s2c) = session_keys(&kem_shared, x_shared.as_bytes(), &transcript);

    let mut writer = SealedWriter::new(write.0, SealKey::new(&c2s));
    let mut reader = SealedReader::new(read.0, SealKey::new(&s2c));

    let mut auth = Vec::with_capacity(PUBLIC_LEN + SIGNATURE_LEN);
    auth.extend_from_slice(identity.public().as_bytes());
    auth.extend_from_slice(&identity.sign(&client_digest(&transcript)));
    writer.write_frame(&auth)?;

    let mut answer = Vec::new();
    if !reader.read_frame_max(&mut answer, MAX_ANSWER_LEN)? {
        return Err(protocol("server closed the connection during login"));
    }
    match answer.split_first() {
        Some((&ACCEPT, flags)) => Ok(Session {
            reader,
            writer,
            peer: server_key,
            options: KeyOptions {
                host: flags
                    .first()
                    .is_some_and(|f| f & FLAG_VM_ONLY != 0)
                    .then_some(false),
                projects: None,
            },
        }),
        Some((&REJECT, message)) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("server refused login: {}", String::from_utf8_lossy(message)),
        )),
        _ => Err(protocol("server sent an invalid login answer")),
    }
}

/// Runs the server side. `authorize` looks the client key up, usually in
/// `authorized_keys`.
pub fn server<S: Duplex>(
    stream: S,
    identity: &Identity,
    authorize: impl FnOnce(&PublicKey) -> io::Result<Option<KeyOptions>>,
) -> io::Result<Session<S>> {
    let mut read = Plain(stream.try_split()?);
    let mut write = Plain(stream);

    // Every read before the login checks out takes only the size it
    // expects, so an unauthenticated caller cannot make the server hold
    // more.
    let mut hello = Vec::new();
    if !read.read_frame_max(&mut hello, CLIENT_HELLO_LEN)? || hello.len() != CLIENT_HELLO_LEN {
        return Err(protocol("client hello has the wrong size"));
    }
    let (magic, rest) = hello.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(protocol("not a cmdterm remote client"));
    }
    let (client_x, rest) = rest.split_at(X25519_LEN);
    let kem_ek_bytes = &rest[..KEM_EK_LEN];

    let x_secret = x25519_secret()?;
    let x_public = x25519_dalek::PublicKey::from(&x_secret);
    let x_shared = x_secret.diffie_hellman(&x25519_public(client_x));
    if !x_shared.was_contributory() {
        return Err(protocol("client sent a weak X25519 key"));
    }
    let kem_ek_array = ml_kem::Key::<ml_kem::EncapsulationKey<MlKem768>>::try_from(kem_ek_bytes)
        .map_err(|_| protocol("bad ML-KEM key"))?;
    let kem_ek = ml_kem::EncapsulationKey::<MlKem768>::new(&kem_ek_array)
        .map_err(|_| protocol("bad ML-KEM key"))?;
    let (kem_ct, kem_shared) = kem_ek.encapsulate();

    let mut reply = Vec::with_capacity(SERVER_HELLO_LEN);
    reply.extend_from_slice(x_public.as_bytes());
    reply.extend_from_slice(&kem_ct);
    reply.extend_from_slice(&random::<NONCE_LEN>()?);
    reply.extend_from_slice(identity.public().as_bytes());
    let signature = identity.sign(&server_digest(&hello, &reply));
    reply.extend_from_slice(&signature);
    write.write_frame(&reply)?;

    let transcript = transcript(&hello, &reply);
    let (c2s, s2c) = session_keys(&kem_shared, x_shared.as_bytes(), &transcript);
    let mut reader = SealedReader::new(read.0, SealKey::new(&c2s));
    let mut writer = SealedWriter::new(write.0, SealKey::new(&s2c));

    let mut auth = Vec::new();
    if !reader.read_frame_max(&mut auth, PUBLIC_LEN + SIGNATURE_LEN)?
        || auth.len() != PUBLIC_LEN + SIGNATURE_LEN
    {
        return Err(protocol("client login has the wrong size"));
    }
    let (key, signature) = auth.split_at(PUBLIC_LEN);
    let client_key =
        PublicKey::from_bytes(key).ok_or_else(|| protocol("client sent an invalid identity"))?;
    if !client_key.verify(&client_digest(&transcript), signature) {
        return Err(protocol("client signature does not verify"));
    }
    let Some(options) = authorize(&client_key)? else {
        let mut answer = vec![REJECT];
        answer.extend_from_slice(b"key not authorized");
        let _ = writer.write_frame(&answer);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("key {} is not authorized", client_key.fingerprint()),
        ));
    };
    let flags = if options.vm_only() { FLAG_VM_ONLY } else { 0 };
    writer.write_frame(&[ACCEPT, flags])?;
    Ok(Session {
        reader,
        writer,
        peer: client_key,
        options,
    })
}

fn x25519_secret() -> io::Result<x25519_dalek::StaticSecret> {
    Ok(x25519_dalek::StaticSecret::from(random::<32>()?))
}

fn x25519_public(bytes: &[u8]) -> x25519_dalek::PublicKey {
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    x25519_dalek::PublicKey::from(key)
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes)
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha3_256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// What the server signs. All parts have fixed lengths, so plain
/// concatenation is unambiguous.
fn server_digest(client_hello: &[u8], server_signed: &[u8]) -> [u8; 32] {
    hash(&[b"cmdterm-remote-v1 server", client_hello, server_signed])
}

fn transcript(client_hello: &[u8], server_hello: &[u8]) -> [u8; 32] {
    hash(&[b"cmdterm-remote-v1 transcript", client_hello, server_hello])
}

fn client_digest(transcript: &[u8; 32]) -> [u8; 32] {
    hash(&[b"cmdterm-remote-v1 client", transcript])
}

/// Client-to-server and server-to-client keys.
fn session_keys(
    kem_shared: &[u8],
    x_shared: &[u8],
    transcript: &[u8; 32],
) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>) {
    let base = Zeroizing::new(hash(&[
        b"cmdterm-remote-v1 keys",
        kem_shared,
        x_shared,
        transcript,
    ]));
    (
        Zeroizing::new(hash(&[b"c2s", base.as_ref()])),
        Zeroizing::new(hash(&[b"s2c", base.as_ref()])),
    )
}

fn protocol(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FrameRead;

    /// Two connected sockets. TCP rather than a Unix socket pair, so the
    /// tests also run on Windows.
    fn pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (b, _) = listener.accept().unwrap();
        (a, b)
    }

    fn run(
        server_id: Identity,
        client_id: Identity,
        allow: bool,
        trust: bool,
    ) -> (
        io::Result<Session<TcpStream>>,
        io::Result<Session<TcpStream>>,
    ) {
        let (a, b) = pair();
        let server = std::thread::spawn(move || {
            server(b, &server_id, |_| Ok(allow.then(KeyOptions::default)))
        });
        let client = client(a, &client_id, |_| {
            if trust {
                Ok(())
            } else {
                Err(io::Error::other("unknown host"))
            }
        });
        (client, server.join().unwrap())
    }

    #[test]
    fn both_sides_agree_and_talk() {
        let server_id = Identity::from_seed(&[1; 64]);
        let client_id = Identity::from_seed(&[2; 64]);
        let server_pub = server_id.public().clone();
        let client_pub = client_id.public().clone();
        let (client, server) = run(server_id, client_id, true, true);
        let mut client = client.unwrap();
        let mut server = server.unwrap();
        assert_eq!(client.peer, server_pub);
        assert_eq!(server.peer, client_pub);
        client.writer.write_frame(b"ping").unwrap();
        let mut buf = Vec::new();
        assert!(server.reader.read_frame(&mut buf).unwrap());
        assert_eq!(buf, b"ping");
        server.writer.write_frame(b"pong").unwrap();
        assert!(client.reader.read_frame(&mut buf).unwrap());
        assert_eq!(buf, b"pong");
    }

    #[test]
    fn unauthorized_clients_are_refused() {
        let (client, server) = run(
            Identity::from_seed(&[1; 64]),
            Identity::from_seed(&[2; 64]),
            false,
            true,
        );
        let error = client.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(server.is_err());
    }

    #[test]
    fn untrusted_servers_never_see_the_client_key() {
        let (client, server) = run(
            Identity::from_seed(&[1; 64]),
            Identity::from_seed(&[2; 64]),
            true,
            false,
        );
        assert_eq!(client.err().unwrap().to_string(), "unknown host");
        // The client hung up before sending its login.
        assert!(server.is_err());
    }

    /// A caller that has not logged in cannot make the server read, or
    /// allocate, more than a client hello.
    #[test]
    fn a_long_hello_fails_before_its_body() {
        let (mut a, b) = pair();
        let server_id = Identity::from_seed(&[1; 64]);
        let result = std::thread::spawn(move || {
            server(b, &server_id, |_| Ok(Some(KeyOptions::default()))).map(|_| ())
        });
        // Only the length of a frame as large as the transport allows.
        a.write_all(&(crate::transport::MAX_FRAME as u32).to_be_bytes())
            .unwrap();
        let error = result.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_tampered_server_hello_fails() {
        let (a, b) = pair();
        let (c, d) = pair();
        let server_id = Identity::from_seed(&[1; 64]);
        std::thread::spawn(move || server(d, &server_id, |_| Ok(Some(KeyOptions::default()))));
        // A relay that flips one bit of the server's ephemeral key.
        std::thread::spawn(move || {
            let mut from_client = Plain(b.try_clone().unwrap());
            let mut to_client = Plain(b);
            let mut from_server = Plain(c.try_clone().unwrap());
            let mut to_server = Plain(c);
            let mut buf = Vec::new();
            from_client.read_frame(&mut buf).unwrap();
            to_server.write_frame(&buf).unwrap();
            from_server.read_frame(&mut buf).unwrap();
            buf[0] ^= 1;
            to_client.write_frame(&buf).unwrap();
        });
        let error = client(a, &Identity::from_seed(&[2; 64]), |_| Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "server signature does not verify");
    }
}
