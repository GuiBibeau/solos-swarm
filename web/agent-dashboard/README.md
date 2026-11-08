# agent-dashboard

A web dashboard of [agent mode](../../docs/agent-mode/README.md)'s tickets, for one person on a phone or a laptop:
what the agent is working on, what it was asked, everything it replied, and what it spent. It also lists the external
jobs the Watchdog checks. It is a Next.js app (App
Router, TypeScript) on Vercel at `https://agent.guibibeau.com`, behind **Sign in with Vercel**. It is not part of the
Rust build.

```text
the box (accepts no inbound connection)           Vercel
 solos-agent  --writes-->  agent.db
 solos-agent-publish --reads (read-only)-->        private Blob store  <--reads/writes (server side)--  this app  <--  you
   builds JSON snapshots, uploads the ones          tickets/index.json                               Sign in with Vercel,
   whose content changed, over HTTPS                tickets/<ticket id>.json                         one allowed email
   and carries the web chat's inbox into the        jobs/index.json
   agent's state (web/inbox.json --> triggers)      notes/index.json
                                                    web/inbox.json
```

The publisher is `solos-swarm agent publish` on the box
([production.md, Dashboard](../../docs/agent-mode/production.md#dashboard)). The app never reaches the box and the box
never serves a request: a web message goes into `web/inbox.json` in the private store, the publisher turns it into a
Trigger the agent already knows how to read (`kind: jsonl`), and the agent's replies come back in the published ticket
snapshots. `notes/index.json` carries the Router's latest decisions on `web:` Threads, so the chat can show the agent's
note (an Ignore's reason, for example) on a message that became no Ticket instead of leaving it without a reply. Discord
and GitHub stay their own sources; the web chat is one more way in, not a replacement.

The composer needs a signed-in, allowed person: the browser never holds the Blob token. `POST /api/chat` validates the
thread and the text, writes the message with the server's token, and `GET /api/chat?t=<thread>` returns the thread's
ticket and the command state for the 2-second poll.

## Pages

The site is one command center in the swarm's own field: matte carbon, hairline panels, Geist for words and
JetBrains Mono for figures. An unlabelled rail on the left (a bottom bar on a phone) carries the five destinations.

| Path | What |
|---|---|
| `/` | **Chat, the home.** A live conversation with the agent: the selected thread in the middle, the recent web conversations on the left, and what the swarm is doing on the right (the field, the active Ticket's stage and worker, what needs you, the external jobs). It polls every 2 s, sends through the server, and keeps the thread in the address and in local storage. It also shows each worker's reports, so the work is visible in the thread. |
| `/command` | **Command.** The full field (every Ticket a node, a running worker its satellite, the host at the centre), what needs you, the four numbers, and the latest tickets beside the external jobs. It warns when the index or the Watchdog is fifteen minutes old. It reloads every 20 s. |
| `/tickets` | **Tickets.** Open and recently ended tickets; each row carries who asked and from where, its age or duration, its worker and its spend. |
| `/tickets/<id>` | **One ticket.** The request, the conversation, the worker's reports, the scope, the facts and budget, the browser's pages, and the decisions; a link to the original Discord message or GitHub issue where one can be made. An open ticket reloads every 10 s. |
| `/swarm` | **Swarm.** The full field and every worker, with the numbers. |
| `/jobs` | **Jobs.** Every external job: how it is checked, its state (`up`, `down`, `stalled`, low on disk), how long it has had that state, and what the last check found. |
| `/login` | "Sign in with Vercel". |

**External jobs.** With `watchdog.jobs` in agent.yaml, the Watchdog writes `jobs/index.json` and the pages above read
it ([runtime.md, External jobs](../../docs/agent-mode/runtime.md#external-jobs)). Jobs in trouble come first; the
Command page shows the three that need a look and links to `/jobs` for the rest. The command center warns when the
Watchdog last looked more than fifteen minutes ago, since then the statuses may be old.

## Preview it locally

```sh
DASHBOARD_FIXTURES=1 npm run dev
```

reads the JSON fixtures in `tests/fixtures/` (written by `tests/agent_publish.rs`) instead of the Blob store, and skips
sign-in, so every screen can be designed and checked without a deployment. The mode requires `NODE_ENV !==
"production"`, so it cannot open a production build.

Discord replies link to ticket pages: `https://agent.guibibeau.com/tickets/<id>`. A link opened before the publisher
has uploaded that ticket says so and checks again by itself.

## How sign-in works

The app uses the OAuth 2.0 authorization code flow with PKCE, then OpenID Connect, as
[Vercel documents it](https://vercel.com/docs/sign-in-with-vercel/getting-started):

1. `/auth/login` makes a `state`, a `nonce` and a PKCE verifier, keeps them in a short-lived encrypted cookie, and
   redirects to `https://vercel.com/oauth/authorize` with the S256 challenge and scope `openid email profile`.
2. Vercel redirects to `/auth/callback`. The callback checks `state` against the cookie and sends nothing to Vercel
   before it matches. It exchanges the code at `https://api.vercel.com/login/oauth/token` with the verifier, then
   verifies the ID token: the signature against `https://vercel.com/.well-known/jwks`, the issuer, the audience (this
   app's client id), the expiry and the `nonce`.
3. The token's `email` must be in `ALLOWED_EMAILS`, compared case-insensitively. Only then is a session cookie set.
   Anything else goes back to `/login?error=...`, and no session is made.
4. The session is an encrypted JWE (`A256GCM`, with a key derived from `SESSION_SECRET`) in an `HttpOnly`, `Secure`,
   `SameSite=Lax` cookie that lasts a week. The app keeps no Vercel token. The sign-in only learns who you are.
5. `/auth/logout` (POST) clears the cookie.

Every request is checked twice. `proxy.ts` redirects anyone without a valid session before anything renders. Every page
asks for a viewer again on the server (`lib/viewer.ts`), and the functions that read the Blob store take a viewer as an
argument, so a page cannot read data without one. The allowlist is checked on every request, not only at sign-in, so
removing an address from `ALLOWED_EMAILS` ends that person's access at once. A deployment missing any setting answers
503 to everything. It never falls back to open.

Messages are untrusted text: a model wrote them, from whatever it read. They are rendered with `react-markdown`
without raw HTML. Links open with `rel="noopener noreferrer nofollow"`. The app never loads an image. It shows it as a
link. The Content-Security-Policy, with a nonce per request set in `proxy.ts`, allows no remote image, frame or script.

## Environment variables

| Variable | Where it comes from | What |
|---|---|---|
| `VERCEL_APP_CLIENT_ID` | you, Production only | The Sign in with Vercel app's client id (`cl_...`). |
| `VERCEL_APP_CLIENT_SECRET` | you, Production only, sensitive | One of the app's client secrets. |
| `SESSION_SECRET` | you, Production only, sensitive | Encrypts the cookies. At least 32 characters (`openssl rand -base64 32`). Changing it signs everyone out. |
| `ALLOWED_EMAILS` | you, Production only | Who may sign in: Vercel account emails, separated by commas. They are compared in lower case. |
| `PUBLIC_URL` | you, Production only | The origin the app is reached at, with no trailing slash: `https://agent.guibibeau.com`. The callback is `${PUBLIC_URL}/auth/callback`. |
| `BLOB_STORE_ID` | connecting the Blob store to the project | Which store to read. With it, `@vercel/blob` reads with the deployment's short-lived OIDC token, so the project holds no Blob token. Connecting the store also adds `BLOB_WEBHOOK_PUBLIC_KEY`, which the app does not use. |
| `BLOB_READ_WRITE_TOKEN` | you, local development only | The store's read-write token, to run the app against the real store from your machine. The deployed app does not have it. |

`.env.example` lists them for local development.

## Set up on Vercel

The deployed app lives in the team `guivercelpro`, as the project `agent-dashboard`, on the domain
`agent.guibibeau.com`. The domain `guibibeau.com` is already in the team. The project has no Git connection: you deploy
it with the CLI from `web/agent-dashboard`. Nothing below is automated. Do it once.

1. **Create and link the project.** From `web/agent-dashboard`:

   ```sh
   vercel link --yes --scope guivercelpro --project agent-dashboard
   ```

   The framework, Next.js, is detected. The build is `npm run build` with the defaults, and only the production
   deployment is used.

2. **Create the private Blob store and connect it.** In the Vercel dashboard, open the project's **Storage** tab, then
   **Create Storage**, **Blob**. Name it `solos-agent-tickets`, set access to **Private**, and connect it to the project.
   Connecting adds `BLOB_STORE_ID` and `BLOB_WEBHOOK_PUBLIC_KEY` to Production and Preview. The store's page also shows
   its read-write token (`vercel_blob_rw_...`). The box uploads with that token: store it there as
   [production.md, Turn on the dashboard](../../docs/agent-mode/production.md#turn-on-the-dashboard) says. Do not add it
   to the project.

3. **Create the Sign in with Vercel app.** Open the team's **Settings**, **Apps**, **Create**. Name it
   `Agent dashboard`, with the slug `agent-dashboard`. Then, on its **Manage** page:

   - **Authentication**: generate a client secret and keep `client_secret_post` enabled. The app sends the secret in
     the token request's form, as Vercel's example does. Copy the secret now, because it is shown once.
   - **Authorization Callback URLs**: add `https://agent.guibibeau.com/auth/callback`. For local development, also add
     `http://localhost:3000/auth/callback`.
   - **Permissions**: enable `openid`, `email` and `profile`. Leave `offline_access` off, because the app wants no
     refresh token.
   - **General, Sign-In Access** (optional, a second lock): choose **Members of the team**, so that only people in
     `guivercelpro` can finish a sign-in. `ALLOWED_EMAILS` still decides who gets in.

   Copy the client id (`cl_...`) from the same page.

4. **Add the environment variables**, for Production only. A preview deployment then answers 503, which is right,
   because its URL is not a registered callback.

   ```sh
   vercel env add VERCEL_APP_CLIENT_ID production                         # paste cl_...
   vercel env add VERCEL_APP_CLIENT_SECRET production --sensitive         # paste the secret
   openssl rand -base64 32 | vercel env add SESSION_SECRET production --sensitive
   printf '%s' '<your Vercel account email>' | vercel env add ALLOWED_EMAILS production
   printf '%s' 'https://agent.guibibeau.com' | vercel env add PUBLIC_URL production
   ```

   For more than one person, separate the emails with commas: `a@example.com,b@example.com`.

5. **Add the domain.**

   ```sh
   vercel domains add agent.guibibeau.com agent-dashboard
   ```

   If `guibibeau.com` uses Vercel's nameservers, Vercel makes the record. Otherwise add a `CNAME` for `agent` to
   `cname.vercel-dns.com` where its DNS is.

6. **Deploy.** From `web/agent-dashboard`, run `vercel deploy --prod`. Leave Deployment Protection at its default. It
   covers the `*.vercel.app` URLs, and the app does its own sign-in on the custom domain. A visit to the project's
   `vercel.app` address that gets past it is redirected to `PUBLIC_URL` to sign in.

7. **Check it.** Open `https://agent.guibibeau.com`. It redirects to the sign-in page. Sign in with the allowed account
   and you land on the ticket list, which says "Nothing has been published yet" until the publisher on the box has run
   once. Any other Vercel account sees "That Vercel account is not allowed here."

## Rotate things

- **The client secret.** The app can hold two. Generate a second one on its **Manage** page. Run
  `vercel env rm VERCEL_APP_CLIENT_SECRET production`, add the new one with `--sensitive`, and redeploy
  (`vercel deploy --prod`). Then delete the first secret.
- **The session secret.** Replace `SESSION_SECRET` and redeploy. Every session and every sign-in in flight ends.
- **Who may sign in.** Edit `ALLOWED_EMAILS` and redeploy. A session whose address left the list stops working at once.
- **The Blob token.** Rotate it on the store and store the new one on the box
  ([production.md, Rotate the dashboard's secrets](../../docs/agent-mode/production.md#rotate-the-dashboards-secrets)).
  The app reads with OIDC, so the project needs no change.

## Development

```sh
cp .env.example .env.local     # fill it in; PUBLIC_URL=http://localhost:3000
npm install
npm run dev                    # http://localhost:3000
npm run lint                   # eslint, no warnings allowed
npm run typecheck
npm test                       # vitest: the sign-in's checks, the session, the allowlist, the proxy, the pages
npm run build
```

The tests need no network and no Vercel. The sign-in tests stand in for Vercel with a key pair, a token endpoint and a
cookie jar (`tests/helpers.ts`). The page tests render the snapshots in `tests/fixtures`, which the Rust crate writes
from a store a scripted agent made, so they are what `agent publish` really builds. To refresh them after you change
`src/agent/publish/snapshot.rs`, run this from the repository root, on the box that builds Rust:

```sh
PUBLISH_FIXTURE_DIR=web/agent-dashboard/tests/fixtures cargo test --test agent_publish -- fixtures
```

`lib/types.ts` is the TypeScript side of `src/agent/publish/snapshot.rs`, and, for the jobs, of `src/agent/external_jobs.rs`.
Both carry the format version `v`, and the app refuses a file whose version it does not know. `tests/fixtures/jobs.json`
is written by the same command.
