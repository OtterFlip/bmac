<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 security

Threat model, authentication, HTTPS, process separation, and secrets. Backup
appliance security is in [`BACKUP.md`](BACKUP.md); this document covers the
parts it depends on.

## Threat model

What BMAC defends against:

- **A network attacker** between the browser and a host, including at first
  contact. Countered by HTTPS with an operator-verified certificate
  fingerprint.
- **Stolen browser credentials.** Countered by never letting the browser keep
  any: the login token lives only in the page's memory, so stolen cookies,
  browser storage, or a copied browser profile hold nothing that opens BMAC.
  Tokens are also opaque, short-lived, and revocable.
- **Cross-site requests.** No ambient credentials exist (no cookies), so
  another site's page can't make authenticated requests.
- **A bug in the web layer.** Countered by running it unprivileged, with only
  a narrow typed interface to the root service.
- **Injection** through form fields, external-tool output, or guest data.
  Countered by typed parameters, argv-only process execution, and escaped
  output.
- **A fully compromised cluster** (root on every host). It must not be able to
  destroy, alter, or read historical backups, or log in to an appliance
  (`BACKUP.md`).
- **A leaked ISO.** Its token is single-use and expires; its host key is only
  that host's web identity.

What BMAC does not try to defend against:

- **Root on one Proxmox host attacking the other hosts.** In a Proxmox
  cluster, root on any node can read `/etc/pve/priv` and use cluster SSH, so
  it already controls the cluster. BMAC adds no boundary between hosts and
  does not claim one.
- **A compromised appliance or a stolen, unlocked appliance pool.** Each
  appliance protects its own data, and serves another appliance only if its
  administrator made that one a trusted peer (`BACKUP.md`, "Trusted peers").

## Browser HTTPS

**Certificates.** The Deployer generates one certificate per host while
building that host's ISO:

- ECDSA P-256 key, self-signed, valid for 10 years. Rotation is supported, as
  described below.
- Subject alternative names: the short host name, the internal FQDN, the
  Tailscale MagicDNS name (`<host>.<tailnet>.ts.net`), and the private IP
  address. The Tailscale `100.x` address is assigned when the host joins
  Tailscale and isn't known at build time, so the documented URL uses the
  MagicDNS name.
- The private key is written to `/etc/bmac/tls/` on the host with root-only
  permissions. The HTTPS front end receives it through systemd
  `LoadCredential`, so the unprivileged user never needs file access to it. It
  is never written to pmxcfs or logs.
- The public certificate is published to
  `/etc/pve/priv/bmac/hosts/<host>/https.pem`.

**Verification.** The Deployer shows the SHA-256 fingerprint of the
certificate's DER encoding on the host card. On first visit the browser warns
that the certificate is not trusted. The operator opens the browser's
certificate viewer, compares the SHA-256 fingerprint with the host card, and
proceeds only if they match. The documentation gives the steps for Chrome,
Firefox, and Safari, and a browser-independent check:

```bash
openssl s_client -connect mox1.example.ts.net:8443 </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256
```

**The Deployer checks first, the browser check is still required.** When a
newly installed host appears on the tailnet, the Deployer opens a TLS
connection to the Dashboard URL, compares the certificate's SHA-256
fingerprint with the one it generated, and closes the connection. It sends
no HTTP request and doesn't log in. On a mismatch it shows a warning, explains
the possible causes (the wrong machine answering at that name, or
interception), and withholds the Dashboard link. On a match it shows the
link, and still tells the operator to compare the fingerprint in the browser
before logging in. The Deployer's connection and the browser's are separate
connections, possibly over different network paths and resolvers. One can be
intercepted while the other isn't, so a passed Deployer check proves
nothing about the browser's connection.

The Settings page also lists every host's SSH host key fingerprint, which a
workstation that didn't install a host uses to pin its first SSH connection
(`DEPLOYMENT.md`, "Workstation SSH").

Once one host is verified, its Dashboard's Settings page lists every host's
fingerprint, read from pmxcfs. The operator can verify the remaining hosts
against that list instead of finding each host card.

**Listener.** `bmacd` listens on port 8443 on the Tailscale and private
addresses only. The host guard keeps it off the public interface. A public
listener is not supported. Network reachability is never treated as
authentication.

The engine tells the front end which addresses to use: this host's
Tailscale addresses (`tailscale status`) and its corosync ring addresses
(`/etc/pve/corosync.conf`). Only RFC 1918, Tailscale's `100.64.0.0/10`, and
IPv6 unique-local addresses are ever offered, and the front end refuses a
wildcard address, so a misconfigured ring can't put the Dashboard on a
public address. The list is refreshed every 30 seconds, so a Tailscale
address that appears after startup gets a listener.

TLS 1.3 and 1.2 only, with ECDHE key exchange (X25519, P-256, P-384) and
AEAD ciphers (AES-GCM, ChaCha20-Poly1305). The front end logs the
certificate's SHA-256 fingerprint at startup.

**Rotation.** Settings, then Certificates: generate a new key and certificate
on the host, show the new fingerprint in the Dashboard (already verified over
the old certificate), and switch over when the operator confirms. Expiry is
monitored (`MONITORING.md`).

Rotation applies to the host the browser is connected to, because that is
the connection the operator has verified; another host is rotated from its
own Dashboard. The engine writes the new pair as `https.next.key` and
`https.next.pem` beside the current one, with the current certificate's
names and a fresh 10-year validity. The switch names the fingerprint the
operator was shown, so a pair replaced in the meantime is refused. The
engine keeps the old pair as `https.previous.*`, moves the new one into
place, publishes the new fingerprint to pmxcfs, and restarts the front end,
which reads its certificate only at startup. The browser then warns again;
the operator compares the fingerprint with the one the Dashboard showed and
logs in again, since the token lived only in the old page's memory. Each
step is audited as `certificate_changed`.

Appliances follow the same pattern with their own certificate and appliance
card (`DEPLOYMENT.md`).

## Login

**Proxmox accounts.** `bmacd` authenticates users through the local Proxmox
API (`POST /api2/json/access/ticket` on `127.0.0.1:8006`), including Proxmox's
second-factor challenge when the user has TOTP, WebAuthn, or recovery keys set
up. BMAC keeps no password database. Proxmox's realms, password policy, and
two-factor settings apply.

TOTP and recovery keys pass through this API as plain values. WebAuthn is
bound to the web origin it was registered for, and Proxmox's WebAuthn
settings name a single origin (normally its own GUI on port 8006). Whether a
WebAuthn challenge can be completed through BMAC's origin on port 8443 must be
tested in the lab; until it is proven, BMAC supports TOTP and recovery keys
and says so on the login page.

The engine makes that call over TLS, trusting only this node's own Proxmox
certificate (`/etc/pve/local/pveproxy-ssl.pem`, else `pve-ssl.pem`). When
Proxmox answers with a second-factor challenge, the engine keeps Proxmox's
partial ticket and gives the browser only an opaque challenge ID, bound to
the browser's address, usable once, valid for two minutes, and good for
three wrong codes before the password is needed again. BMAC keeps nothing
else Proxmox returns: the Proxmox ticket is discarded.

**Roles** come from Proxmox group membership. The engine reads it from
`/etc/pve/user.cfg`, which also says whether the account is enabled or has
expired; a user's own Proxmox ticket may not be allowed to read their groups
through the API.

| Role | Proxmox group | May |
| --- | --- | --- |
| Admin | `bmac-admin` (and always `root@pam`) | Everything, including destructive operations and settings |
| Operator | `bmac-operator` | Start and answer non-destructive operations; view everything |
| Viewer | `bmac-viewer` | View only |

Each entry in the operation registry declares its minimum role. A user in no
BMAC group cannot log in. The same minimum role applies to answering a job's
request, cancelling a job, resuming an interrupted one, and marking one
failed. An answer relayed from another host is checked against that minimum
role again on the host running the job. A forced cancel, which terminates a
script mid-step, needs Admin whatever the operation. Every launch, answer,
cancel request, resume, and mark-failed is written to the audit log with the
user and their address; a repeated launch with the same idempotency key is
not a second launch. Scripts run with a cleared environment holding only
`PATH`, `LANG`, `HOME`, and the run ID. Any secret answer of three characters
or more is replaced with `[redacted]` in every later event, before the event
leaves the supervisor.

**Tokens: memory only, a hard rule.** A successful login returns an opaque
token: 256 random bits, not a JWT, meaningless outside the engine's session
table. The web app keeps it in one JavaScript variable inside its API module
and nowhere else:

- no cookies of any kind (the server sets none, for any purpose);
- no `localStorage`, `sessionStorage`, IndexedDB, Cache Storage, or service
  worker;
- never in a URL, so never in history, logs, or `Referer` headers;
- not exposed to other code in the page except through the API module.

It is sent only as `Authorization: Bearer <token>`. Consequences, all
intended:

- Reloading the page, closing the tab, or opening a new tab means logging in
  again. Each tab is its own session.
- The app routes pages in the browser so normal navigation never reloads the
  document. While a job is waiting for the user's answer, the app warns before
  the tab is closed or reloaded.
- File downloads (LUKS headers, join bundles, SSH snippets, exported logs)
  are fetched with the token and handed to the browser as a `Blob`; there are
  no authenticated plain links.
- Live updates are read with `fetch` streaming, because `EventSource` can't
  send the header (`ARCHITECTURE.md`, "Live updates").

The same rule applies to the appliance's web UI. The Deployer never logs in
to `bmacd` and never calls its API.

A token in page memory can still be read by script running in that page, so
the Content Security Policy below matters: no inline script, no third-party
script, and all script served by `bmacd` itself.

**Sessions** (server side):

- The engine stores each session (token hash, user, role, host, created,
  last used) in host-local SQLite. The front end never stores tokens.
- They expire after 30 minutes without a user request, or 12 hours absolute;
  both are configurable. An open event stream doesn't count as activity.
- The engine re-checks the user's Proxmox account and groups every 5 minutes
  and ends the session if the user was disabled or removed from the groups.
  An ended session closes its event streams at once.
- Sessions are per host. Logging in to another host is a separate login; all
  operational state is shared anyway.
- "Log out everywhere" for a user writes a revocation timestamp to pmxcfs.
  Every host ends that user's older sessions.

- A user whose role changes in Proxmox is logged out at the next check and
  logs in again with the new role.
- The revocation is `/etc/pve/priv/bmac/sessions/revoked/<hash of user>.json`;
  every host checks it every 10 seconds.

Login attempts are rate-limited per source address (30 attempts in 5
minutes) and per user name, compared without case (10 failures in 15 minutes,
from any address). A limited attempt never reaches Proxmox. Every login,
logout, failure, refusal, rate limit, and ended session is an audit event.

## Requests

- Every API request needs the bearer token. Because the browser never
  attaches it automatically, cross-site request forgery has nothing to ride
  on, and no CSRF token is needed. As defense in depth, state-changing
  requests must be `POST` with `Content-Type: application/json`, and the front
  end rejects any `Origin` other than its own.
- No CORS headers are sent, so other origins can't read responses.
- Parameters are typed and validated against the operation's schema. The
  browser never supplies a command line, path, dataset name, or flag.
- Destructive operations need the same confirmations as v2 (`GO`,
  `DESTROY prodN`, `REMOVE moxN`, `moxN IS PERMANENTLY DISCONNECTED`, ...),
  checked by `bmacd`.
- Each launch carries the resource revision the dialog was opened from. A
  stale revision gets "state changed, review again" rather than acting.
- Response headers: a strict Content Security Policy (`script-src 'self'`,
  no `unsafe-inline` or `unsafe-eval`, `connect-src 'self'`). The app page
  adds a style nonce, new on every load, because xterm and Radix insert
  `<style>` elements at run time; the page tags those with it. Inline
  `style` attributes stay blocked, so the console uses palette colors only;
  `X-Content-Type-Options: nosniff`; `frame-ancestors 'none'`;
  `Cache-Control: no-store` on every API response;
  `Referrer-Policy: no-referrer`. No HSTS, since the certificate is
  self-signed.
- Request bodies are size-limited.
- Every piece of external output (command output, guest data, names) is
  rendered as text. React escapes by default; the code never uses
  `dangerouslySetInnerHTML` for external data, and a lint rule enforces it.

## Process separation

| Process | User | Can |
| --- | --- | --- |
| `bmacd` HTTPS front end | `bmac-web`, no shell, no sudo | Terminate TLS, serve the web app, relay API requests and event streams |
| `bmacd` engine | root | Everything BMAC does |
| `bmac-source` (backup source endpoint, `BACKUP.md`) | `bmac-source`, with ZFS `send`, `hold`, and `release` delegated on guest datasets only | Stream BMAC snapshots to enrolled appliances |
| `bmac-hook` | root (Proxmox runs it) | Lifecycle admission and cleanup queueing |

The engine, the front end, and `bmac-source` all come from the one `bmacd`
binary and package. The front end runs as its own systemd unit
(`bmacd-web.service`, bound to `bmacd.service` so they start and stop
together), and `bmac-source` as `bmac-source.service`, bound the same way.
The front end talks to the engine over `/run/bmac/engine.sock`, and
`bmac-source` over `/run/bmac-source/engine.sock`, a separate directory only
its group may enter, with its own fixed set of calls: the enrolled
appliances (name, certificate fingerprint, and address), and one call per
appliance request, naming the certificate it came with. `bmac-source`
listens on nothing; it dials each approved appliance's address
(`BACKUP.md`, "Connections"). The engine checks the peer's UID with
`SO_PEERCRED` on both.
Every request carries the caller's token, and the engine resolves the user
and role itself, so a compromised front end can act only within sessions it
sees. The request set is fixed: the API's typed calls and event
subscriptions. There is no generic command.

The front end's systemd unit is hardened: `NoNewPrivileges`,
`ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, an
empty capability set, `RestrictAddressFamilies` limited to IP and Unix, and a
`SystemCallFilter`.

## Host to host

`bmacd` instances talk to each other over TLS 1.3 on port 8444, on the
private and Tailscale addresses. Each keeps a live stream open to every peer
for observation changes, job events, and log copying (`ARCHITECTURE.md`,
"Cluster-wide view"). Both sides authenticate with the Proxmox node certificate
(`/etc/pve/nodes/<node>/pve-ssl.pem` and its key), verified against
`/etc/pve/pve-root-ca.pem`. The peer's node name must be a current cluster
member. These are Proxmox-managed credentials (`pvecm updatecerts`). BMAC adds
no PKI of its own inside the cluster. Proxmox issues node certificates for
server use only, so the server accepts the server-auth key usage on a client
certificate. Membership is checked on every request, not just at the
handshake: the certificate must be valid for exactly one current member's
name, and a request from anything else is refused with 403. A client
checks the server's certificate for the member's name it dialed.

**Joins and enrollment** use the same port. A joining host or a new appliance
has no node certificate yet, so 8444 also accepts a client without one, for
exactly two requests: `POST /v1/join` (a new host's join token, `DEPLOYMENT.md`)
and `POST /v1/enroll` (an appliance's enrollment token and client
certificate, `BACKUP.md`). The server sends the CA after its node
certificate, so the client can find in the chain the CA whose SHA-256
fingerprint its bundle names. It then verifies the node certificate against
that CA and the member's name before sending the token. The
token is single-use, checked against its hash and expiry in pmxcfs, and
rate-limited per source address. A valid token only creates a pending
request; nothing is trusted until an administrator approves it in the
Dashboard. Every other request from a client without a member certificate is
refused.

Approving a join lets the cluster reach the new host as root with the
join's setup key, an Ed25519 key generated per bundle. Its private half is
kept in `/etc/pve/priv/bmac/joins/` and copied, mode 0600, only into the
approving job's own work directory. SSH pins the host key the new host
reported in its request, which the administrator compared with the
Deployer's host card when approving. Before the job runs anything there, it
checks that the new host's bootstrap file carries this cluster's UUID and
the token issued for that slot. The new host checks the approving host's
8006 certificate fingerprint before it asks for the `root@pam` password for
`pvecm add`, so the password goes nowhere else. Proxmox merges root's
`authorized_keys` into the cluster-wide file when a host joins, so the setup
key is removed from both right after the join. The bundle, token, request,
and key are deleted when the job finishes, and revoking the bundle deletes
them early.

## Secrets

| Secret | Where it is | Where it never goes |
| --- | --- | --- |
| Proxmox root password | Typed into the Deployer; only the hash goes into the ISO | Logs, pmxcfs, Deployer settings |
| Guest root passwords (production, staging) | Hashes in `/etc/pve/priv/bmac/secrets/` | Plaintext anywhere after entry |
| LUKS passphrase | Typed at a console; held briefly in a root-only tmpfs file during setup or disk work | ISO, Dashboard, logs, pmxcfs, any network path |
| Tailscale auth keys | Typed into the Deployer; single-use key embedded in one ISO; digest kept against reuse | Logs, pmxcfs (digest only) |
| Discovery OS SSH key and host key | Generated by the Deployer per discovery boot; the private client key stays in Deployer memory | Anywhere after the discovery session |
| Host SSH host key (Ed25519) | Generated by the Deployer per ISO, as in v2; private key embedded in that ISO only, public key pinned in the workstation's `~/.ssh/known_hosts` | Deployer settings, logs, any other ISO |
| Login tokens | Engine session table (hash only) and the page's memory | Cookies, browser storage, URLs, logs |
| Host HTTPS private key | `/etc/bmac/tls/` on its host | pmxcfs, other hosts, logs |
| Postmark token (cluster) | `/etc/pve/priv/bmac/secrets/postmark.token` | Pages, logs, exports, ISOs |
| Appliance client key and admin password hash | The appliance only | The cluster |
| Restore grants | Issued by an appliance, short-lived, held in memory by the job using them | Logs |
| Appliance trusted-peer list (peer fingerprints, not secret) | The appliance's metadata dataset, changed only on its own UI | The cluster, other appliances |

Secret parameters are a distinct type in the operation schema. Their values
go to a process's stdin or stay in memory; they are never part of a stored
event. Captured command output passes through a redaction filter for known
secret shapes before it is stored. Tests feed known secrets through every
workflow and check that none appears in stored events, logs, or pages.

## Audit

Every job admission, request answer, cancellation, settings change, login
event, enrollment, and certificate change is stored as an audit event with
the user, source address, host, and time. Audit events are copied to peers
like other logs, and are the last to be evicted (`MONITORING.md`). They are
diagnostic records, not tamper-proof evidence against root on every host.
