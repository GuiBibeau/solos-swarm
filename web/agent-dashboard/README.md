# agent-dashboard

A web dashboard of [agent mode](../../docs/agent-mode/README.md)'s tickets, for one person on a phone or a laptop:
what the agent is working on, what it was asked, everything it replied, and what it spent. It is a Next.js app (App
Router, TypeScript) that lives on Vercel at `https://agent.guibibeau.com`, behind **Sign in with Vercel**, and it is
not part of the Rust build.

```
the box (accepts no inbound connection)           Vercel
 solos-agent  --writes-->  agent.db
 solos-agent-publish --reads (read-only)-->        private Blob store  <--reads (server side)--  this app  <--  you
   builds JSON snapshots, uploads the ones          tickets/index.json                           Sign in with Vercel,
   whose content changed, over HTTPS                tickets/<ticket id>.json                     one allowed email
```

The publisher is `solos-swarm agent publish` ([production.md](../../docs/agent-mode/production.md#dashboard)). This
app only reads what it uploaded. Nothing here can reach the box, and the box never serves a request.

## Pages

| Path | What |
|---|---|
| `/` | Open tickets first, then the latest ended: title, stage, who asked and from where, age, spend, the worker's state, and what a ticket waits for. It says when it was last synced and warns when the publisher has gone quiet. It reloads its data every 20 s. |
| `/tickets/<id>` | The request, the conversation in order (what was said to the ticket and every reply the agent sent, rendered as markdown), the worker's reports, the browser's pages, the decisions, the budget, and a link to the original Discord message or GitHub issue where one can be made. An open ticket reloads every 10 s. |
| `/login` | "Sign in with Vercel". |

Ticket pages are what Discord replies link to: `https://agent.guibibeau.com/tickets/<id>`. A link opened before the
publisher has uploaded that ticket says so and checks again by itself.

## How sign-in works

OAuth 2.0 authorization code with PKCE, then OpenID Connect, as
[Vercel documents it](https://vercel.com/docs/sign-in-with-vercel/getting-started):

1. `/auth/login` makes a `state`, a `nonce` and a PKCE verifier, keeps them in a short-lived encrypted cookie, and
   redirects to `https://vercel.com/oauth/authorize` with the S256 challenge and scope `openid email profile`.
2. Vercel redirects to `/auth/callback`. The callback checks `state` against the cookie (nothing is sent to Vercel
   before it matches), exchanges the code at `https://api.vercel.com/login/oauth/token` with the verifier, then
   verifies the ID token: signature against `https://vercel.com/.well-known/jwks`, issuer, audience (this app's
   client id), expiry, and the `nonce`.
3. The token's `email` must be in `ALLOWED_EMAILS` (case-insensitively). Then, and only then, a session cookie is set.
   Anything else goes back to `/login?error=...`, and no session is made.
4. The session is an encrypted JWE (`A256GCM`, key derived from `SESSION_SECRET`) in an `HttpOnly`, `Secure`,
   `SameSite=Lax` cookie that lasts a week. No Vercel token is kept: the sign-in only learns who you are.
5. `/auth/logout` (POST) clears the cookie.

Every request is checked twice. `proxy.ts` redirects anyone without a valid session before anything renders, and
every page asks for a viewer again on the server (`lib/viewer.ts`), and the functions that read the Blob store take a
viewer as an argument, so a page cannot read data without one. The allowlist is checked on every request, not only at
sign-in: taking an address out of `ALLOWED_EMAILS` ends that person's access immediately. A deployment missing any
setting answers 503 to everything; it never falls back to open.

Messages are untrusted text (a model wrote them, from whatever it read). They are rendered with `react-markdown`
without raw HTML; links open with `rel="noopener noreferrer nofollow"`; an image is never loaded, it is shown as a
link; and the Content-Security-Policy (a nonce per request, set in `proxy.ts`) allows no remote image, frame or script.

## Environment variables

| Variable | What |
|---|---|
| `VERCEL_APP_CLIENT_ID` | The Sign in with Vercel app's client id (`cl_...`). |
| `VERCEL_APP_CLIENT_SECRET` | One of the app's client secrets. |
| `SESSION_SECRET` | Encrypts the cookies; at least 32 characters, `openssl rand -base64 32`. Changing it signs everyone out. |
| `ALLOWED_EMAILS` | Who may sign in: Vercel account emails, comma separated. Here: `g.bibeaulaviolette@gmail.com`. |
| `PUBLIC_URL` | The origin the app is reached at, no trailing slash: `https://agent.guibibeau.com`. The callback is `${PUBLIC_URL}/auth/callback`. |
| `BLOB_READ_WRITE_TOKEN` | The private Blob store's token. Connecting the store to the project on Vercel adds it, and also `BLOB_STORE_ID`, with which the SDK reads with a short-lived OIDC token instead (it prefers that when both are there). The app only reads. |

`.env.example` has them all.

## Setup on Vercel

The team is `guivercelpro`, the domain `guibibeau.com` is already in it. Nothing below is automated: run it once.

**1. The project.** From `web/agent-dashboard`:

```sh
vercel link --yes --scope guivercelpro --project agent-dashboard
```

Or import the repository in the dashboard and set **Root Directory** to `web/agent-dashboard` (the framework, Next.js,
is detected). The build is `npm run build`, with the defaults; the project needs only its production deployment.

**2. The Blob store, private.** Dashboard: the project's **Storage** tab, **Create Storage**, **Blob**, set access
to **Private**, name it `solos-agent-tickets`, connect it to the project. Or:

```sh
vercel blob create-store solos-agent-tickets --access private --yes
```

(`--yes` connects it to the linked project in every environment.) Then `vercel env ls` should show `BLOB_STORE_ID`
and, for a store made from the dashboard, `BLOB_READ_WRITE_TOKEN`. The same read-write token is what the box
uploads with: **Storage**, the store, then the token (or `vercel env pull .env.local` and copy
`BLOB_READ_WRITE_TOKEN`; delete the file after). Store it on the box as
[production.md](../../docs/agent-mode/production.md#dashboard) says.

**3. The Sign in with Vercel app.** Team **Settings**, **Apps**, **Create**: name `Agent dashboard`, slug
`agent-dashboard`. Then on its **Manage** page:

- **Authentication**: generate a client secret and keep `client_secret_post` enabled (the app sends the secret in the
  token request's form, as the docs' example does). Copy the secret now; it is shown once.
- **Authorization Callback URLs**: add `https://agent.guibibeau.com/auth/callback`. For local development also
  `http://localhost:3000/auth/callback`.
- **Permissions**: enable `openid`, `email` and `profile`. Leave `offline_access` off: no refresh token is wanted.
- **General, Sign-In Access** (optional, a second lock): **Members of the team**, so that only people in
  `guivercelpro` can finish a sign-in at all. `ALLOWED_EMAILS` still decides who gets in.

Copy the client id (`cl_...`) from the same page.

**4. The environment variables**, for Production only (a preview deployment then answers 503, which is right: its
URL is not a registered callback):

```sh
vercel env add VERCEL_APP_CLIENT_ID production            # paste cl_...
vercel env add VERCEL_APP_CLIENT_SECRET production        # paste the secret
openssl rand -base64 32 | vercel env add SESSION_SECRET production
printf 'g.bibeaulaviolette@gmail.com' | vercel env add ALLOWED_EMAILS production
printf 'https://agent.guibibeau.com' | vercel env add PUBLIC_URL production
```

**5. The domain.**

```sh
vercel domains add agent.guibibeau.com agent-dashboard
```

If `guibibeau.com` uses Vercel's nameservers the record is made for you; otherwise add a `CNAME` for `agent` to
`cname.vercel-dns.com` where its DNS is.

**6. Deploy.** `vercel deploy --prod` from `web/agent-dashboard`. (Leave Deployment Protection at its default: it covers
the `*.vercel.app` URLs, and the app does its own sign-in on the custom domain. A visit to the project's `vercel.app`
address that gets past it is redirected to `PUBLIC_URL` to sign in.) Open `https://agent.guibibeau.com`: it redirects to
the sign-in page. Signing in with the allowed account lands on the ticket list ("Nothing has been published yet"
until the publisher on the box has run once). Signing in with any other Vercel account shows "That Vercel account is
not allowed here."

## Rotating things

- **Client secret.** The app can hold two. Generate a second on its Manage page, `vercel env rm VERCEL_APP_CLIENT_SECRET
  production` and add the new one, redeploy (`vercel deploy --prod`), then delete the first.
- **Session secret.** Replace `SESSION_SECRET` and redeploy: every session and sign-in in flight ends.
- **Who may sign in.** Edit `ALLOWED_EMAILS` and redeploy. A session whose address left the list stops working at once.
- **The Blob token.** Rotate it on the store, update the box's token
  ([production.md](../../docs/agent-mode/production.md#rotating-the-dashboards-secrets)), and, if the app uses
  `BLOB_READ_WRITE_TOKEN` rather than OIDC, update it in the project and redeploy.

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

The tests need no network and no Vercel: the sign-in tests stand in for Vercel with a key pair, a token endpoint and a
cookie jar (`tests/helpers.ts`). The page tests render the snapshots in `tests/fixtures`, which the Rust crate writes
from a store a scripted agent made, so they are what `agent publish` really builds. To refresh them after changing
`src/agent/publish/snapshot.rs`, from the repository root, on the box that builds Rust:

```sh
PUBLISH_FIXTURE_DIR=web/agent-dashboard/tests/fixtures cargo test --test agent_publish -- fixtures
```

`lib/types.ts` is the TypeScript side of `src/agent/publish/snapshot.rs`; both carry the format version `v`, and the app
refuses a file whose version it does not know.
