//! Remote control client: connect to a supervisor's control surface, optionally over
//! TLS with an ed25519 authorized-keys handshake (see docs/remote-management.md).
//!
//! - **Plaintext** (local dev): `--host`/`--port`, no auth — matches a supervisor whose
//!   `control` has no `authorized_keys`.
//! - **Authenticated** (remote): `--profile NAME` resolves host/port + a client identity
//!   key + a pinned server cert from `~/.config/supervisor/config`. The channel is TLS
//!   (server cert pinned, known-hosts style); the client proves its identity by signing
//!   the server's challenge nonce with its ed25519 key.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// A resolved connection target: where, and (if authenticated) with what identity + pin.
pub struct Target {
    pub host: String,
    pub port: u16,
    pub auth: Option<Auth>,
}

pub struct Auth {
    /// ed25519 private key seed (32 bytes), loaded from the profile's `identity` file.
    pub seed: [u8; 32],
    /// Pinned server certificate (DER), from the profile's `server_cert` PEM.
    pub server_cert: Vec<u8>,
}

// ---- profiles config (~/.config/supervisor/config) -------------------------

#[derive(serde::Deserialize)]
struct ConfigFile {
    #[serde(default)]
    profile: std::collections::HashMap<String, ProfileEntry>,
}
#[derive(serde::Deserialize)]
struct ProfileEntry {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    identity: String,
    server_cert: String,
}
fn default_port() -> u16 {
    9000
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SUPERVISOR_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/supervisor/config")
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// Resolve a connection target from an optional profile name and/or explicit host/port.
/// `--profile` (authenticated) takes precedence; otherwise plaintext host:port.
pub fn resolve_target(profile: Option<&str>, host: Option<&str>, port: u16) -> Result<Target> {
    if let Some(name) = profile {
        let path = config_path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading supervisor config {}", path.display()))?;
        let cfg: ConfigFile = toml::from_str(&text).context("parsing supervisor config")?;
        let entry = cfg
            .profile
            .get(name)
            .ok_or_else(|| anyhow!("no profile [{}] in {}", name, path.display()))?;
        let seed = load_identity(&expand_tilde(&entry.identity))?;
        let server_cert = load_pinned_cert(&expand_tilde(&entry.server_cert))?;
        Ok(Target {
            host: entry.host.clone(),
            port: entry.port,
            auth: Some(Auth { seed, server_cert }),
        })
    } else {
        Ok(Target {
            host: host.unwrap_or("127.0.0.1").to_string(),
            port,
            auth: None,
        })
    }
}

/// Load a 32-byte ed25519 seed from an identity file (64 hex chars, whitespace tolerated).
fn load_identity(path: &std::path::Path) -> Result<[u8; 32]> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading identity key {}", path.display()))?;
    let bytes = from_hex(text.trim()).ok_or_else(|| anyhow!("identity key is not valid hex"))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("identity key must be 32 bytes (64 hex chars)"))
}

/// Load the first certificate (DER) from a PEM file — the pinned server cert.
fn load_pinned_cert(path: &std::path::Path) -> Result<Vec<u8>> {
    let pem = std::fs::read(path).with_context(|| format!("reading server cert {}", path.display()))?;
    let mut rd = std::io::BufReader::new(&pem[..]);
    let cert = rustls_pemfile::certs(&mut rd)
        .next()
        .ok_or_else(|| anyhow!("no certificate in {}", path.display()))?
        .context("parsing server cert PEM")?;
    Ok(cert.as_ref().to_vec())
}

// ---- the control round-trip ------------------------------------------------

/// Send one JSON op to a supervisor and return its reply line.
pub fn control_send(target: &Target, op_json: &str) -> Result<String> {
    let addr = (target.host.as_str(), target.port);
    let mut tcp = std::net::TcpStream::connect(addr)
        .with_context(|| format!("connecting to supervisor at {}:{}", target.host, target.port))?;

    match &target.auth {
        None => {
            // Plaintext: send op, read reply.
            tcp.write_all(op_json.as_bytes())?;
            tcp.write_all(b"\n")?;
            tcp.flush()?;
            let mut resp = Vec::new();
            tcp.read_to_end(&mut resp)?;
            Ok(String::from_utf8_lossy(&resp).trim().to_string())
        }
        Some(auth) => authenticated_send(tcp, target, auth, op_json),
    }
}

fn authenticated_send(tcp: std::net::TcpStream, target: &Target, auth: &Auth, op_json: &str) -> Result<String> {
    use ed25519_dalek::{Signer, SigningKey};

    // TLS with the pinned server cert (known-hosts style; hostname not checked).
    let verifier = Arc::new(pin::PinnedServerCert::new(auth.server_cert.clone()));
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // ServerName is required by the API but our verifier pins by cert, not name.
    let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())
        .unwrap_or_else(|_| rustls::pki_types::ServerName::try_from("supervisor").unwrap());
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .context("starting TLS session")?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    // 1. Read the server's challenge: {"nonce":"<hex>"}
    let nonce_line = read_line(&mut tls).context("reading auth challenge")?;
    let nonce = parse_nonce(&nonce_line).ok_or_else(|| anyhow!("bad auth challenge from server"))?;

    // 2. Sign the nonce and send {"pubkey","sig"} then the op (pipelined).
    let sk = SigningKey::from_bytes(&auth.seed);
    let pubkey = sk.verifying_key().to_bytes();
    let sig = sk.sign(&nonce).to_bytes();
    let auth_msg = format!(
        "{{\"pubkey\":\"{}\",\"sig\":\"{}\"}}\n",
        to_hex(&pubkey),
        to_hex(&sig)
    );
    tls.write_all(auth_msg.as_bytes())?;
    tls.write_all(op_json.as_bytes())?;
    tls.write_all(b"\n")?;
    tls.flush()?;

    // 3. Read the reply (server replies then closes; a clean TLS close_notify ends it).
    let mut resp = Vec::new();
    if let Err(e) = tls.read_to_end(&mut resp) {
        // A truncated-close after we have a full reply line is fine.
        if resp.is_empty() {
            return Err(anyhow!("reading reply: {e}"));
        }
    }
    Ok(String::from_utf8_lossy(&resp).trim().to_string())
}

/// Read one '\n'-delimited line from a reader (byte at a time — small, once per handshake).
fn read_line<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            Err(e) => return Err(anyhow!("read: {e}")),
        }
    }
    Ok(line)
}

fn parse_nonce(line: &[u8]) -> Option<[u8; 32]> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let hex = v.get("nonce")?.as_str()?;
    let bytes = from_hex(hex)?;
    bytes.as_slice().try_into().ok()
}

// ---- key generation (for a client identity) --------------------------------

/// Generate an ed25519 keypair; return (seed_hex, pubkey_hex).
pub fn generate_identity() -> (String, String) {
    use ed25519_dalek::SigningKey;
    let mut csprng = rand::rngs::OsRng;
    let sk = SigningKey::generate(&mut csprng);
    (to_hex(&sk.to_bytes()), to_hex(&sk.verifying_key().to_bytes()))
}

// ---- hex + the pinned-cert verifier ----------------------------------------

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}
fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < s.len() {
        out.push((nib(s[i])? << 4) | nib(s[i + 1])?);
        i += 2;
    }
    Some(out)
}

mod pin {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};
    use std::sync::Arc;

    /// Verifies the server by pinning its exact certificate (known-hosts style): the
    /// presented end-entity cert must byte-match the pinned DER. TLS still proves the
    /// server holds the cert's private key (handshake signature), so this is a full
    /// authentication of the server — we just anchor trust to the pin, not a CA/hostname.
    #[derive(Debug)]
    pub struct PinnedServerCert {
        pinned: Vec<u8>,
        provider: Arc<CryptoProvider>,
    }
    impl PinnedServerCert {
        pub fn new(pinned: Vec<u8>) -> Self {
            Self {
                pinned,
                provider: Arc::new(rustls::crypto::ring::default_provider()),
            }
        }
    }
    impl ServerCertVerifier for PinnedServerCert {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            if end_entity.as_ref() == self.pinned.as_slice() {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(Error::General(
                    "server certificate does not match the pinned certificate".into(),
                ))
            }
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider.signature_verification_algorithms.supported_schemes()
        }
    }
}
