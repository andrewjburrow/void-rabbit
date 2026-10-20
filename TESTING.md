# Validation — 0.5

Fifteen automated Rust tests cover:

- compressed/multi-page draw.io parsing, nested coordinates, icon inference, IP metadata, invalid XML and cyclic parents;
- topology/device/group validation, revisions, edge cleanup, and map deletion;
- scheduler eligibility, failure thresholds, transition deduplication, pause policy, stale-result rejection, SMTP alert creation, and probe errors;
- SMTP secret redaction/preservation and actual delivery to a local test relay;
- authenticated topology routes and diagram uploads above the standard JSON limit;
- Windows/Linux ping argument fixtures, IPv4/IPv6 echo output, sub-millisecond latency, timeouts, and Windows unreachable responses that exit zero;
- initial setup key, setup closure, Argon2id hashes, hashed session tokens, cookies, CSRF enforcement, viewer permissions, preference permissions, last-admin protection, disabled users, password/session revocation, login rate limiting, and activity records without secrets;
- atomic device/connection undo, stale-revision rejection, retained monitoring state, multiple undo and the 20-edit limit;
- legacy database migration, outage deduplication/recovery and history across deletion/undo;
- per-account saved views, geometry validation, viewer permissions, live WAL database backups, admin-only download and local admin recovery/session revocation;
- audio format validation, authentication, persistence, MIME type, deletion, and preservation across preference saves.

Browser verification uses Chromium with the real Rust HTTP server and SQLite database. A deterministic ping executable returns up/down results to exercise scheduled checks without relying on network permissions. It verifies initial admin setup, diagram import, UTC clock, dragging over nodes to pan without selecting, click selection, stable DOM during polling, fullscreen/expand, theme persistence, saved device positions, status transitions, one sound per new down event, custom audio persistence, activity history, diagnostics, account creation, viewer UI restrictions, branded login, connection selection/removal/undo, device edits/deletion/undo, device-list navigation, saved map views, persistent NOC mode, outage history, and downloadable SQLite backups.

Browser built-in sound synthesis is instrumented for deterministic transition counts; uploaded audio persistence is checked with a valid WAV fixture. This does not prove physical speaker output.

The actual Linux `recover-admin` executable was also exercised through a controlling terminal: both password prompts hid input, recovery completed, and sessions were revoked.

Rust tests, formatting, and Clippy checks run in Linux. Native Windows execution and Windows Firewall/LAN reachability cannot be exercised in this environment. Windows argument/output behavior is covered by fixtures. Docker is not available here, so its build/runtime is not verified. No claim is made that ICMP works through every router or endpoint firewall.

The optional Linux browser check is included as `tests/browser-smoke.cjs`. The running application does not require Node. To reproduce the browser check after building the debug binary:

```bash
cargo build --locked
npm install
npx playwright install chromium
npm run test:browser
```

The fixture starts its own server with a temporary database and ping executable; it does not use the normal application data directory.
