# supervisor — remote management (authenticated off-box control)

Status: DESIGN, spike-validated 2026-09-13. Priority workstream (gates the held
inbox cutover — the first inbox deploy runs *through* this). Colin's scope call:
**reinvent the auth, reuse TLS for the wire.**

## The goal

An agent or operator connects to a *remote* supervisor — sees the running roster,
adds/removes/applies services, restarts, dumps a chain — **securely, off-box, with
no SSH**. It turns "route every prod step through the manager" into self-service:
edit desired-state on the box from anywhere you're authorized.

## The split (Colin's call)

- **Reuse for the wire:** TLS, terminated **host-side** by theater's tcp handler
  (`upgrade-to-tls-server`, native rustls). The wasm guest does *zero* transport
  crypto — `send`/`receive` just become encrypted. Not the place to reinvent.
- **Build ourselves:** the **authenticated identity layer** — an ed25519
  authorized-keys handshake, SSH-shaped. This is the part worth owning.

## Why this is safe *and* buildable (spike-validated)

- The only guest-side crypto op is ed25519 signature **verify** — *deterministic*,
  no RNG in the path. `ed25519-dalek` (default-features off, verify-only) compiles
  clean to `wasm32-unknown-unknown` (62KB). So the `getrandom`→host-entropy problem
  never arises for v1.
- TLS cert/key live in the tcp handler's manifest config, provisioned at bootstrap;
  the guest never generates or handles certs.

## Protocol

```
client (native CLI)                         supervisor (wasm actor)
  |-- TCP connect ------------------------------>|  accept
  |<== TLS handshake (rustls / host tcp handler)=|  upgrade-to-tls-server(conn)
  |         [ all bytes below are encrypted ]     |
  |<-- challenge: 32-byte nonce -----------------|  (fresh per connection)
  |-- auth: { pubkey, sig = Sign(client_sk, nonce) } -->|
  |                                              |  pubkey ∈ authorized_keys ?
  |                                              |  VerifyingKey(pubkey).verify(nonce, sig) ?
  |<-- ok  (or close on failure) ----------------|
  |         [ existing JSON control ops: list/status/add/remove/apply/restart/chain ]
```

- **authorized_keys**: a set of client ed25519 pubkeys the supervisor accepts,
  in its config (init state). Like `~/.ssh/authorized_keys`. Editable later via a
  control op (a bootstrapped key can authorize more) — v1 may seed-only.
- **nonce freshness**: server-issued per connection. If theater's `random` handler
  is crypto-grade + reachable, source it there; else a monotonic counter is
  acceptable over an already-fresh TLS session for v1 (replay across sessions is
  TLS-mitigated). Decide during build.
- **server identity**: the client pins the server's self-signed cert fingerprint
  (known-hosts style) in its profile — so a MITM can't present a different cert.

## Client: profiles config

`~/.config/supervisor/config` (TOML), SSH-config-shaped:

```toml
[profile.prod-inbox]
host        = "…"          # off-box address
port        = 9000
identity    = "~/.config/supervisor/keys/id_ed25519"   # client private key
server_cert = "~/.config/supervisor/known/prod-inbox.pem"  # pinned server cert
```

`supervisor --profile prod-inbox list` (and `add`/`remove`/`apply`/`restart`/`chain`)
resolves the profile, TLS-connects (pinning `server_cert`), runs the ed25519
handshake with `identity`, then speaks the existing JSON control protocol.

## Bootstrap contract (the manager runs this once, on the VPS)

Someone has to land the first supervisor process; after that, management is remote.
Bootstrap provisions:

1. **Server TLS cert+key** — self-signed, generated on the box (openssl/rcgen).
   Manifest tcp handler: `server_tls = { enabled = true, cert = "<path>", key = "<path>" }`.
   The cert (public) is handed to each client to pin as `server_cert`.
2. **authorized_keys seed** — the *first* authorized client pubkey(s), in the
   supervisor's init config. Chicken-and-egg, exactly like adding your key to
   `authorized_keys` before your first SSH in. **Open question: whose key is the
   first authorized client — the manager's, inbox-dev's, an operator key Colin holds?**
3. **Control bound off-box** — the tcp listener on a reachable interface:port
   (only safe *because* of the auth above; never expose unauthenticated).

## Reuse vs build — the line

| Layer            | Who         | Why                                         |
|------------------|-------------|---------------------------------------------|
| Wire encryption  | rustls (host tcp handler) | reinventing it is pure risk    |
| Client auth      | **us** (ed25519 authorized-keys) | the interesting identity layer |
| Cipher/curve math| audited crates (dalek/RustCrypto) | never hand-roll        |

## Spike results (2026-09-13)

- `upgrade-to-tls-server` + `ServerTlsConfig{enabled,cert,key}` confirmed present
  at theater rev `cfcd737` (our pin).
- `ed25519-dalek` verify-only builds for `wasm32-unknown-unknown` (no getrandom).

## Implementation notes (built + E2E green, 2026-09-13)

- **The tcp handler auto-terminates TLS on accept** when `server_tls.enabled=true` —
  the connection is *already encrypted* by the time `handle-connection` runs. So the
  guest's `upgrade-to-tls-server` returns `"…already TLS"`; we tolerate that (and still
  support a STARTTLS-style handler where the upgrade actually runs). Either way the
  channel is TLS before the ed25519 handshake.
- **Server** (`supervisor/src/lib.rs`): `control.authorized_keys` non-empty flips the
  surface to authed — nonce challenge → verify signature + allowlist membership → op.
  Empty = legacy localhost plaintext (dev). `control.bind` selects the interface.
- **Client** (`cli/src/client.rs`): rustls with a pinned-cert verifier (known-hosts),
  ed25519 signing, profiles in `~/.config/supervisor/config`. `supervisor keygen` mints
  a client identity. Both the client and the embedded theater tcp handler need a
  process-level rustls `CryptoProvider` (ring) installed at startup.
- **Dev/bootstrap manifest**: `supervisor spawn … --tls-cert --tls-key --authorized-key
  --control-bind` emits an authed manifest (server_tls on the tcp handler + authorized_keys
  in the roster). This is both the local test path and the shape the VPS bootstrap uses.
- **E2E verified**: authorized key → op succeeds over TLS; authed mutation reaches the
  op handler; wrong/unauthorized key → rejected; a plaintext client against the TLS port
  gets a TLS alert, not access (no bypass).

## Build order

1. Server handshake in the supervisor actor (accept → upgrade-to-tls-server →
   nonce → verify against authorized_keys → gate the existing `handle_op`).
2. Client TLS + handshake + profiles config in the CLI.
3. Bootstrap helper (cert-gen + authorized_keys seeding), for the manager.
4. E2E: local two-process auth round-trip, then the inbox deploy through it.
