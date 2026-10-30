# SYNC.md — Deploying and testing multi-device sync

`im` is offline-first: each device writes to its own SQLite database, and
`im :sync` exchanges the changed cells with a per-user Durable Object on
Cloudflare (`sync-server/`). This is the runbook for exercising that path end to
end — against a real deployment (§1) and entirely on one machine with Miniflare
(§2).

**Read this first**

- The account commands are `im :sync register|login|status|logout`; `im :sync`
  exchanges events. A bare `im sync` (no colon) is entry text and logs a mood
  named "sync".
- The server address comes from `[sync] server` in the client config and
  defaults to `http://127.0.0.1:8787`. It is **not** stored with the account:
  signing in does not remember it. `IM_SYNC_URL` overrides the config for one
  command, so a stale export wins over the config.
- Sync ships from v0.2.6 on; v0.2.5 and earlier predate it.
- A pre-sync database (v0.2.5) has INTEGER ids and is not migrated. Do not point
  a sync build at an old state dir; move the file aside first (appendix A).
- `im :sync --reset` deletes every local row and rebuilds from the server log.
  It is the repair path for a locally diverged database, not an undo.

---

## 1. Remote test (real Cloudflare deployment)

### 1.1 Deploy the server

Prerequisites: a Cloudflare account and Node 20+. The free Workers plan is
enough: D1 and SQLite-backed Durable Objects are included.

```sh
cd sync-server
npm install
npx wrangler login
npx wrangler whoami          # confirm the right account
```

Create the auth database, then paste the printed `database_id` over the
placeholder in `sync-server/wrangler.toml`:

```sh
npx wrangler d1 create im-sync-auth
# [[d1_databases]] binding = "AUTH_DB" ... database_id = "<paste this>"
```

Apply the schema remotely, sign tokens with a real secret, and deploy:

```sh
npx wrangler d1 migrations apply AUTH_DB --remote     # asks to confirm
openssl rand -base64 48 | npx wrangler secret put JWT_SECRET
npm run deploy                                       # == npx wrangler deploy
```

The output names the URL — the worker is `im-sync`, so it is
`https://im-sync.<your-subdomain>.workers.dev`. Nothing else needs creating:
migration tag `v1` in `wrangler.toml` instantiates the `UserSyncDO` class on
deploy, and each user gets one object via `idFromName(user_id)`.

Smoke-test the gateway before involving a client:

```sh
SERVER=https://im-sync.<your-subdomain>.workers.dev
curl -s -X POST "$SERVER/api/v1/auth/register" \
  -H 'content-type: application/json' \
  -d '{"email":"probe@example.com","password":"correct horse"}'
# 201 {"user_id":"…","email":"probe@example.com","token":"…"}
```

Failures worth recognizing:

| Response | Meaning |
| --- | --- |
| 500 `the auth database is not migrated…` | the `--remote` migration was skipped |
| 409 | that email exists already (there is no account-delete endpoint) |
| 400 `password must be at least 8 characters` | pick a longer one |

If `npm run deploy` rejects the config, `npx wrangler deploy --dry-run`
validates it without uploading. Redeploying later keeps the same URL and the
D1 rows and Durable Object logs behind it; only renaming the worker or moving
to another account changes the URL. Keep the `[[migrations]]` tag `v1` —
migration tags are append-only, and a deploy that drops one fails.

Deploy the server from the same commit that built the clients; the wire format
is small but moving.

### 1.2 Get a sync-capable client onto both devices

| Route | Needs | Second device gets |
| --- | --- | --- |
| Build `main` | Rust toolchain + the sibling `matchmaker` checkout (below) | `target/release/im` copied over |
| Prerelease tag | push access + a self-contained tree (below) | archive from the GitHub release |
| Stable release | same | Homebrew (`brew install Squirreljetpack/tap/im`) or the archive |

Building locally:

```sh
git clone https://github.com/Squirreljetpack/im
cd im
# Cargo.toml patches matchmaker-lib to ../matchmaker/matchmaker-lib
git clone https://github.com/Squirreljetpack/matchmaker ../matchmaker
cargo build --release
./target/release/im :sync status   # "Not signed in (run `im :sync login`)."
```

`[patch.crates-io]` means the im checkout does not build on its own. CI builds a
tag from a bare clone, so the patch has to be gone (publish `matchmaker` first)
or replaced by a vendored copy before a release can build — otherwise
`dist build` fails with `patch location ../matchmaker does not contain packages
matching matchmaker-lib`. Try it on a clean clone before tagging.

Copy `target/release/im` to the other machine (or build there); the model is
compiled into the binary, so the executable is all it needs.

To cut a release instead:

1. Bump `[workspace.package] version` in `Cargo.toml` and refresh `Cargo.lock`.
2. Add a `## [X.Y.Z] - <date>` section to `CHANGELOG.md`. The current
   `[Unreleased]` section only covers the tracker rework, so sync needs its own
   entry or the release notes will not mention it.
3. `git tag vX.Y.Z && git push origin main vX.Y.Z`. The `Release` workflow
   builds and publishes only on tags (there is no manual dispatch) and expects
   the tag to match the `Cargo.toml` version.

A prerelease tag such as `vX.Y.Z-rc.1` produces a GitHub prerelease with the
archives attached, but skips the publish jobs: the Homebrew tap is not updated
(`publish-prereleases` is off) and npm is not published by the workflow at all
(`publish-jobs = ["homebrew"]`; the npm package is only attached as an asset).
For a prerelease, download `im-<target>.tar.xz` (or the Windows `.zip` /
`im-installer.ps1`) from the release page.

### 1.3 Point both devices at the server and sign in

On **each** device, put the server in the client config (`~/.config/im/config.toml`;
`dev.toml` for a debug build):

```toml
[sync]
server = "https://im-sync.<your-subdomain>.workers.dev"
```

Then:

```sh
im :sync status              # "Not signed in (run `im :sync login`)."
```

Device A creates the account; device B joins it:

```sh
# A
im :sync register            # prompts for email + password (8+ chars)

# B
im :sync login               # same email, same password
```

Both print the server they signed in to and suggest `im :sync`. Signing in does
not record the URL: without `[sync] server` (or with `IM_SYNC_URL` set to
something else) the same account talks to whatever that command resolves.
`IM_SYNC_URL` is the per-command override, e.g.
`IM_SYNC_URL=http://127.0.0.1:8787 im :sync` to point one run at a local
server.

A quick two-clients-on-one-machine variant of the whole section: keep the
deployment and use two throwaway state dirs as in §2.2, with `IM_SYNC_URL`
pointing at the deployed worker. That exercises the real Worker/D1/DO path
without a second device.

### 1.4 Exercise it

Baseline replication, A → B:

```sh
# A
im happy . first remote sync test
im ! water the plants
im :sync
# Pushed <n> change(s); applied <n>; created <n> row(s); deleted <n>.
# Up to date at cursor <n>.

# B
im :sync
im @                 # water the plants is there
im                   # the mood shows up in today's journal
```

Reverse direction:

```sh
# B
im calm . from the other device
im ! buy milk
im :sync

# A
im :sync             # applied <n>; created 2 row(s)
im @                 # both tasks
```

Concurrent edits: change the same task body on both devices while offline (the
TUI, or `im +<short-id>`), then `im :sync` A and then B. The server picks the
winning cell, both devices converge, and the device whose write lost prints a
notice of the form `… body: yours (…) was replaced by … (<device> at …)`. There
is no text-conflict prompt any more — only deletions can ask a question.

Deletion: `im :tool clear` (confirm the prompt) removes today's moods and their
tracker rows; the delete events go out on the next `im :sync`. The other device
prints `deleted <n>` and the entries disappear. If you had unsynced edits to
something the other side deleted, the edit is dropped with a notice and an
interactive `im :sync` offers to re-create the content as a copy (tasks and
moods).

Offline tolerance:

```sh
IM_SYNC_URL=http://127.0.0.1:9999 im :sync
# Failed to reach the sync server at http://127.0.0.1:9999   (exit 1)
im :sync status      # signed in locally; the queue only shows when reachable
```

The outbox is kept: sync again with the right URL and the queued events go.
Stamps come from the device clock, so a device far ahead of the server has its
writes refused; `im :sync` reports it and retries once with fresh stamps.

Rebuild from the server:

```sh
im :sync --reset     # interactive: confirm, local rows are dropped and replayed
im @                 # everything the server has is back, including A's rows
im :sync status      # same account; device id rotated
```

### 1.5 When it goes wrong

- `npx wrangler tail` (from `sync-server/`) streams worker logs.
- `npx wrangler secret list` confirms `JWT_SECRET` is set.
- `npx wrangler d1 execute im-sync-auth --remote --command \
  "SELECT id, email, created_at FROM users"` lists registered accounts.
- The per-user event log lives in the user's Durable Object (`UserSyncDO`):
  Workers & Pages → `im-sync` → Durable Objects → the object for that user.
- Client side: `im :sync status` (account + queued count), and `im.log` in the
  state dir (appendix A) for the last run's debug output; `-v` raises verbosity.

### 1.6 Teardown

```sh
cd sync-server
npx wrangler delete              # worker + every user's Durable Object storage
npx wrangler d1 delete im-sync-auth
```

Local databases on each device are untouched.

---

## 2. Local test (no Cloudflare account)

Everything here runs against Miniflare: `wrangler dev` boots a local workerd
with local D1 and Durable Object storage under `sync-server/.wrangler/state`.
No account, no deployment, no network.

### 2.1 Automated suites

Backend (Worker + DO) inside workerd:

```sh
cd sync-server
npm install
npm test            # wrangler types && vitest run
npm run typecheck
```

Client end to end. The suite starts its own `wrangler dev` on a free port with a
temporary `--persist-to` directory, applies the D1 migrations, then drives the
client engine through registration, replication, conflicts, deletes, pagination,
retries, offline behaviour and `--reset`:

```sh
# from the repo root; install the sync-server deps first if you have not
(cd sync-server && npm install)   # the suite shells out to `npx wrangler`
cargo test --test sync_integration
```

It does not need a dev server already running and does not touch
`sync-server/.wrangler`. Unit-level client tests live in `src/sync/tests.rs` and
`src/db/`.

### 2.2 Manual: one server, two clients

Terminal 1 — the server:

```sh
cd sync-server
npm install
npm run migrate:local     # once: applies migrations to .wrangler/state
npm run dev               # wrangler dev → http://127.0.0.1:8787
```

Terminal 2 — device A:

```sh
export XDG_STATE_HOME=/tmp/im-device-a
im :sync register
im happy . hello from a
im ! try local sync
im :sync
```

Terminal 3 — device B:

```sh
export XDG_STATE_HOME=/tmp/im-device-b
im :sync login            # the account A just created
im :sync
im @                      # A's task is here
```

No URL override is needed when the config leaves `[sync] server` alone:
`127.0.0.1:8787` is the default. If §1.3 put a deployed URL in the config, run
these with `IM_SYNC_URL=http://127.0.0.1:8787` (or a separate dev config). Notes:

- `XDG_STATE_HOME` is the Linux switch for "another device on this machine"
  (state is `$XDG_STATE_HOME/im/im.db`). On macOS, `dirs` has no state dir and
  falls back to `$HOME/.local/state`, so `export HOME=/tmp/im-device-a` gives
  the same isolation (in a throwaway shell). On Windows use two user accounts or
  a VM.
- The configuration is separate from state and shared by both "devices": the
  trackers and moods in `~/.config/im/config.toml` apply to both, which is what
  you want when testing tracker slot races.
- Debug builds use `im.dev.db`; release builds use `im.db`. Both are fresh under
  a new state dir.

### 2.3 Reset the local server

Stop `wrangler dev`, then:

```sh
rm -rf sync-server/.wrangler/state
npm run migrate:local
```

That wipes local D1 (accounts) and every Durable Object's log. To keep the
server state but start over as a client, register a new email instead.

Or run the whole thing against a throwaway persistence directory:

```sh
cd sync-server
npx wrangler d1 migrations apply AUTH_DB --local --persist-to /tmp/im-sync-local
npx wrangler dev --persist-to /tmp/im-sync-local
```

---

## Appendix A — Where things live

| What | Path |
| --- | --- |
| Client database (release) | `$XDG_STATE_HOME/im/im.db`, else `~/.local/state/im/im.db` |
| Client database (debug) | same directory, `im.dev.db` |
| Client config | `~/.config/im/config.toml` (debug: `dev.toml`) |
| Sync server | `[sync] server` in the client config; `IM_SYNC_URL` overrides |
| Client log | `<state dir>/im.log` |
| Server local state | `sync-server/.wrangler/state` (gitignored) |
| Server config | `sync-server/wrangler.toml` |
| Server tests | `sync-server/test/`, `tests/sync_integration.rs` |

The device id, pull cursor and last emitted stamp are in `_sync_state`; the
signed-in account and bearer token are in `_sync_account` (tokens last 30 days).
The local outbox is `_sync_events`. A reset rotates the device id but keeps the
account.
