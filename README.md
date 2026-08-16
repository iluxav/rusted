# rusted

[![CI](https://github.com/iluxav/rusted/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/iluxav/rusted/actions/workflows/ci.yml)
[![Release](https://github.com/iluxav/rusted/actions/workflows/release.yml/badge.svg)](https://github.com/iluxav/rusted/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/iluxav/rusted?color=ff6b24&label=release)](https://github.com/iluxav/rusted/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0%20%2F%20AGPL--3.0-c47b45)](LICENSE.md)

<p align="center">
  <img src="crates/rusted-server/assets/rusted-logo2.png" width="160" alt="rusted logo">  
</p>
<p align="center" style="color:#e66201; ">Rusted</p>

A microfunction platform where a tiny JavaScript file becomes a live HTTP endpoint — or a live MCP server — in seconds, executed by QuickJS inside a restricted Rust runtime.

`rusted becomes the MCP server that lets agents write their own tools`

```bash
curl -fsSL https://raw.githubusercontent.com/iluxav/rusted/main/install.sh | sh
```

Prebuilt binaries cover Apple Silicon and Linux (x86_64 and arm64). Anywhere else, including Intel Macs: `cargo install --path crates/rusted-cli`.

Then develop a function locally — nothing else to install, no server or database needed:

```bash
rusted run index.js        # http://127.0.0.1:7400/f/<name>, hot reload
```

To run the platform itself (server, console, storage):

```bash
make db                                  # postgres:18 via docker compose (port 5457)
cargo build --release
./target/release/rusted serve &          # functions on :7411, admin + console on :7412

export RUSTED_ADMIN=http://127.0.0.1:7412
```

That last line matters: the CLI talks to the hosted service at `https://rusted.sh`
unless told otherwise, so without it `push` and friends would deploy to a server
that isn't yours. `RUSTED_ADMIN` points them at your own; `--admin <url>` does it
for a single command.

### Deploying

```bash
cat > greet.js <<'EOF'
export default async function handler(request, context) {
  const input = await request.json();
  return context.json({ message: `Hello, ${input.name}` });
}
EOF

rusted push greet.js --name greet        # → http://127.0.0.1:7411/f/greet
                                         # (imports are bundled on the way)
curl -X POST http://127.0.0.1:7411/f/greet -d '{"name":"Ada"}'
# {"message":"Hello, Ada"}

rusted preview greet.js --ttl 120        # temporary endpoint, expires on its own
rusted invoke greet --input '{"name":"Bob"}'
rusted logs greet                        # recent invocations with console output
rusted list | rusted pull greet | rusted verify greet.js | rusted delete greet
```

Scripts can carry their own deployment intent, so `rusted push api.js` needs no flags:

```js
export const http = {
  name: "api",
  methods: ["GET", "POST"],
  path: "/users/{id}",
};

export default async function handler(request, context) { ... }
```

Explicit flags override the file's `http` export. Unknown keys fail verify — typos can't deploy silently. Being real code (not comments), the declaration survives bundling.

One more key that export takes: `public: true` exempts the function from `--require-auth`. An OAuth callback or a webhook target is called by a third party that cannot present your API key, so the function itself declares that keyless callers are expected — the auth gate consults the stored record, never anything the caller sent. On a server that doesn't require auth, every function is reachable anyway and the flag changes nothing. (`export const mcp` has taken `public` since MCP functions landed; it now passes the same gate.)

### MCP functions

The other surface a file can export: tools instead of a request handler. The same push makes it a live MCP server at the same `/f/<name>` URL:

```js
export const mcp = {
  name: "slugger",
  tools: {
    slugify: {
      description: "Turn a title into a URL slug",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
      async handler({ text }) {
        return text.toLowerCase().replace(/[^a-z0-9]+/g, "-");
      },
    },
  },
};
```

You write handlers; the platform speaks the protocol. `initialize` and `tools/list` are answered from deploy-time metadata, arguments are validated against each tool's `inputSchema` before any sandbox boots, and a thrown error comes back as an `isError` tool result the model can read and retry — never a protocol error. Return a string for text content; any other value is sent as JSON text, and an object result is mirrored in `structuredContent` too (the spec types it as an object, so arrays and scalars travel as text only).

Hosted end-user tools can delegate caller authentication to an external OAuth authorization server:

```js
export const mcp = {
  name: "notes",
  auth: {
    type: "oauth",
    issuer: "https://app.example.com",
    audience: "https://rusted.sh/f/notes",
    scopes: ["folders:read"],
  },
  tools: {
    whoami: {
      description: "Show the verified caller",
      inputSchema: { type: "object" },
      handler(_args, context) {
        return context.auth;
      },
    },
  },
};
```

Rusted publishes protected-resource metadata, challenges unauthenticated clients, discovers the issuer's authorization-server metadata, and introspects each bearer token — authenticating the introspection call with vault-held client credentials (`introspectionClientIdSecret` / `introspectionClientSecretSecret`, HTTP Basic) that the function itself can never read. Issuer, exact audience, expiry, required scopes, and revocation status must all validate before a sandbox starts. Tool code receives only sanitized `context.auth` fields (`subject`, `clientId`, optional `connectionId`, and `scopes`); the bearer token is never exposed to JavaScript or forwarded downstream. OAuth issuers are HTTPS **origins** — no path, no trailing slash — which rules out path-style issuers (Keycloak realms, Azure tenants) and Auth0's trailing-slash issuer for now; the audience must match the token's `aud` exactly. Introspection cannot redirect, both discovery and introspection use the runtime's public-network SSRF guard, authorization-server metadata is cached briefly, rejected tokens are negatively cached by hash, and an unreachable issuer is a `503` — distinct from an invalid token's `401` challenge — so clients are never told to re-consent because a server blipped. Rejected tokens appear in the owner's logs as refusals.

`public: true` and `auth` are mutually exclusive. Without either, the existing owner-key behavior remains unchanged. OAuth is authorization-server agnostic: the browser consent and identity system belong to the application publisher, not to Rusted.

The declaration is checked at verify time, so a module that won't serve can't deploy: tool names are 1–64 chars of `a-z 0-9 - _` (`word_count`, not `wordCount`), at most 32 tools, and a tool entry is exactly `description`, `inputSchema`, and `handler` — spec extras like `title`, `annotations`, or `outputSchema` are rejected rather than silently dropped. An mcp module must not have a default export; tools are the interface.

The push prints its deliverable — the block to paste into a client:

```
deployed mcp function slugger (rev 1)

add to your MCP client config:
{
  "mcpServers": {
    "slugger": {
      "url": "http://127.0.0.1:7411/f/slugger",
      "headers": { "Authorization": "Bearer <your rusted api key>" }
    }
  }
}
```

The endpoint demands an API key of the owner unless the file says `public: true` or declares external `auth`. A tool call is one invocation under your plan's limits; `initialize` and `tools/list` are free. A module exports `http` or `mcp`, never both — pushing one over the other switches the function's kind.

What an mcp function is not:

- **No SSE or streaming.** Every request gets one JSON response; a `GET` for the server-initiated stream gets a `405`, as the spec prescribes for a server that offers none.
- **No sessions.** The `Mcp-Session-Id` header is echoed back, not backed by stored state.
- **No `listChanged` notifications.** The server never speaks first; the tool list changes only when you push.
- **No unbounded work.** A tool call runs under your plan's execution budget like any invocation.

The dev loop is the http one: `rusted run index.js` serves the tools locally with hot reload and prints a config block of its own — same shape, minus the auth header, since local serving is trusted, plus a note that the pushed endpoint will want your key. Connect a client, edit, push when it works. `rusted new my-tools --mcp` scaffolds a starting point, and [examples/mcp-server](examples/mcp-server) is a complete file.

This is distinct from the platform's own MCP server on the admin port (`/mcp`), whose tools — execute, deploy, list, delete, inbox*create, inbox_read — an agent uses to build on rusted. An mcp \_function* is what such an agent (or you) deploys: it serves the tools in the file, under the owner's key and limits.

## Signing in

```bash
rusted login
```

Prints a short code, you approve it once in a browser, and the CLI stores the key it's granted in `~/.config/rusted/credentials.json` (owner-readable only). Nothing to copy, and the key it mints appears in the console's **API keys** page like any other — revoke it there if you lose the machine.

Resolution order is `--api-key` → `RUSTED_API_KEY` → that file, so CI keeps using an environment variable and needs no browser.

## Console, auth, and API keys

The web console lives on the admin port (http://127.0.0.1:7412). Sign-in is real GitHub OAuth: create an OAuth app at github.com/settings/developers with callback `http://127.0.0.1:7412/auth/github/callback`, then start the server with `RUSTED_CONSOLE_GITHUB_CLIENT_ID` and `RUSTED_CONSOLE_GITHUB_CLIENT_SECRET` set (the bare `GITHUB_*` names still work as a fallback; the login page shows these instructions until you do). The `RUSTED_CONSOLE_` prefix is deliberate: this is _platform_ configuration, distinct from any identically-named tenant secret in the vault — give the console its own dedicated GitHub app whose single callback URL nothing else ever repoints.

API keys are minted in the console (shown once; only a hash is stored). `rusted serve --require-auth` makes every function endpoint demand `Authorization: Bearer rk_live_…`. Key verification is served from an in-memory cache invalidated over Postgres LISTEN/NOTIFY — revocation propagates in milliseconds without per-request DB reads.

Everything lives in Postgres (`DATABASE_URL`, default `postgres://rusted:rusted@127.0.0.1:5457/rusted`; connect directly with `psql` on port 5457).

Configuration comes from the environment, and `rusted` loads a `.env` from the working directory at startup — `cp .env.example .env`, fill in the GitHub credentials, done. `.env` is gitignored.

Every command takes `--json` for stable machine-readable output.

## What's here

| Path                   | Purpose                                                                                                                                                                               |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/rusted-engine` | QuickJS executor: uncatchable wall-clock interrupt, heap cap, output cap, structured `console` logs, fresh context per invocation                                                     |
| `crates/rusted-server` | Orchestrator: data API (`/f/<name>`, `/r/<id>`), admin API for the CLI, content-addressed Postgres store with immutable revisions, inboxes, per-function concurrency 1, temp-run TTLs |
| `crates/rusted-cli`    | The single `rusted` binary                                                                                                                                                            |

The engine choice (QuickJS over Boa) came out of a measured spike — cold start, throughput, memory, and whether hostile code can actually be stopped. QuickJS won on all four, decisively on the last: it can interrupt a runaway script uncatchably and cap its heap, which Boa cannot.

## Starting a new function

`rusted new` scaffolds a directory that is ready to run:

```bash
rusted new my-fn           # TypeScript (the default)
cd my-fn
rusted run index.ts        # develop at http://127.0.0.1:7400/f/my-fn
```

What lands in it: `index.ts` with a typed handler, `rusted.d.ts` describing exactly what the runtime has (and nothing it doesn't — no DOM, no Node), a strict `tsconfig.json` wired to those declarations, a `package.json` with `dev` and `deploy` scripts, and a `.gitignore`. No install step — `rusted run` bundles in-process, and `npm i <pkg>` works the moment you import something.

Two flags change the shape:

```bash
rusted new my-fn --js          # plain JavaScript: just index.js, no tsconfig or declarations
rusted new my-tools --mcp      # an MCP server (export const mcp with a sample tool) instead of an HTTP handler
```

The directory name becomes the function's declared name, so `npm run deploy` (or `rusted push index.ts`) publishes it as `/f/my-fn` without further flags.

## Developing a function

`rusted run` serves one function locally with hot reload — no server, no database, no API key:

```bash
rusted run index.js        # http://127.0.0.1:7400/f/<name>
```

That's the whole loop. If your handler has imports, rusted bundles it in process with [rolldown](https://rolldown.rs) — ESM, ES2020, no Node builtins — watches the directory, and rebuilds and reloads on every save. No node, npx, or esbuild involved, and nothing is written to disk. A file with no imports is served directly. Point `npm run dev` at it and you never think about it again:

```jsonc
"scripts": { "dev": "rusted run index.js" }
```

Each request prints its outcome, timings, and `console.log` output to the terminal. When your handler throws, local dev returns the real message **and a JS stack mapped back through the bundle to your own files** — `at handler (index.js:12:14)`, not `handler:2919`. Deployed functions still return a generic 500 to callers, since those aren't yours. A failed build or a file that won't compile leaves the last working version serving.

You still need `npm install` for the packages you import — rolldown resolves them from `node_modules` like any bundler. What you don't need is a JavaScript toolchain to run rusted.

When you're ready to deploy, `rusted build` produces the artifact with the same bundler:

```bash
rusted build index.js            # → dist/index.js, then: rusted push dist/index.js
rusted build index.js --sourcemap
```

It refuses to write a bundle that wouldn't deploy — no handler, or code that won't load — and prints the route the file declares.

### Secrets while developing

`rusted run` has no database and no master key, so there is nothing to decrypt: a module that declares [`config.secrets`](#secrets) starts with a note saying so, and `context.env` stays `undefined`. It's typed as optional for exactly this reason — TypeScript points at every unguarded read before it fails locally. The pragmatic pattern is an explicit fallback:

```js
const clientSecret = context.env?.GITHUB_CLIENT_SECRET ?? "dev-placeholder";
```

which develops against the placeholder and picks up the real value once deployed. To exercise the real path end to end, run the full server locally — `rusted serve` with Postgres and `RUSTED_SECRETS_KEY` set — and its console has the same Secrets page, injecting exactly as the deployed one does.

### Limits while developing

Local runs get the most permissive plan's limits — 30s execution, 25 outbound calls — so nothing blocks you mid-thought. Each run then reports what it _would_ cost:

```
✓ 200 POST /convert/1   wall 1550ms · exec 1502ms
  ⚠ needs Pro — 1502ms exec over 1000ms on Dev
```

With `RUSTED_API_KEY` set, rusted looks up your actual plan in the background — never blocking startup — and the warning names it directly: `over your Dev plan: 1502ms exec over 1000ms`.

Useful flags for `run`: `--port`, `--exec-ms` (execution budget), `--outbound` (fetch calls allowed per invocation), and `--build 'your command'` to replace the built-in bundling with your own pipeline. To develop against a specific tier, set both: `--exec-ms 1000 --outbound 2` is the Dev plan.

## Controlling the response

`context.json` and `context.text` take an optional second argument:

```js
return context.json(
  { queued: true },
  {
    status: 202,
    headers: { "cache-control": "no-store", "x-request-id": id },
  },
);
```

Status must be 200–599. Headers that frame the response — `content-length`, `transfer-encoding`, `connection`, and friends — belong to the platform and are refused, as is any value containing a line break. A refused header fails the call rather than shipping a half-correct response.

## Receiving: inboxes

A function can call out. Nothing can call _in_ — which rules out OAuth callbacks, webhooks, form submissions, and anything a third party has to initiate. That's especially true of an agent running in a browser or someone's cloud: it has no address at all.

An inbox is a throwaway URL that accepts a POST from anyone and holds what arrives:

```bash
rusted inbox new stripe-data --ttl 2m
# https://rusted.sh/inbox/435007f5f71dc851a66e39aedb5b4e43ebdadb5ae12c878a
#   anyone with this URL can POST to it; reading needs your key
#   expires in 120s
```

**The write address and the read handle are deliberately different things.** The URL is unguessable and grants exactly one capability: writing. Reading is by name and needs your key. So handing the URL to Stripe never hands over what Stripe sent — and that separation is what makes it safe to paste a receiving URL into someone else's dashboard.

Read it three ways. From the CLI:

```bash
rusted inbox get stripe-data     # also: inbox list, inbox rm <name>
```

From inside a deployed function, scoped to whoever deployed it:

```js
export default async function handler(request, context) {
  const messages = await context.inbox.get("stripe-data");
  const total = messages.reduce((sum, m) => sum + (m.amount ?? 0), 0);
  return context.json({ payments: messages.length, total_cents: total });
}
```

Or over MCP as `inbox_create` and `inbox_read`, which is how an agent uses it — create an inbox, hand out the URL, poll until something lands.

Scoping comes from the function's stored owner, never from the name asked for, so a handler can't name its way into another account's inbox.

### How arrivals accumulate

```bash
rusted inbox new events   --ttl 1h                      # keep every message
rusted inbox new oauth-cb --ttl 2m --store upsert --drain
```

`--store append` (the default) keeps everything; `upsert` keeps only the most recent, which is what you want for a single value like an OAuth code. `--drain` removes the inbox on the first read that finds something, like taking a message off a queue.

Both defaults are the non-lossy choice, because `upsert` silently discards earlier writes and `--drain` silently discards on read. Note that `--drain` is at-most-once: if the read fails in transit the message is gone, which is fine for a code you can request again and wrong for a payment event.

### What it costs and when it ends

The TTL runs **from creation and is never extended by activity** — sliding expiry would let anyone holding the write URL keep the inbox, and its storage, alive indefinitely. When it's over the URL answers `410 Gone`, which tells a well-behaved webhook sender to stop retrying rather than escalating to a disabled endpoint. Expired, drained, and never-existed are all the same `410`, so probing addresses reveals nothing. An inbox that's alive but empty answers `200` with no messages, so a polling agent can tell "nothing yet" from "too late".

A public write endpoint is an unbounded write primitive unless it's bounded, so: 64KB per message, 100 messages, and 1000 accepted writes over an inbox's life. Bodies that aren't valid UTF-8 are refused rather than stored mangled.

Messages are served from memory and written through to Postgres on the same call, so a restart loses nothing and other server instances are told to reload over the same `LISTEN/NOTIFY` channel everything else uses. Expiry deletes the payload rather than merely hiding it — a TTL on something holding webhook data is a promise it stops existing.

> `context.inbox` is only present on a deployed function. `rusted run` lends no host services, so a handler that uses it works deployed and fails locally; it's typed as optional so that's visible before you ship.

## Secrets

Credentials don't belong in source — a deployed function's code is stored, revisioned, and visible in the console. A secret is set once, encrypted, and injected only into functions that ask:

```js
export const config = {
  secrets: ["GITHUB_CLIENT_SECRET", "OAUTH_COOKIE_KEY_CURRENT"],
};

export default async function handler(request, context) {
  const secret = context.env.GITHUB_CLIENT_SECRET;
  // …
}
```

Set the values in the console under **Secrets**. Names are env-style — `A-Z`, `0-9`, `_`, not starting with a digit — and a value is never shown again after saving; re-enter it to rotate.

**Asking is the grant.** A function that declares no `config.secrets` gets no `context.env` at all, even if the account holds secrets — so a handler can't quietly read credentials it never declared, and what a function can see is visible in its source. Declared names are all-or-nothing: if one isn't set, the invocation is refused before the handler runs, with the missing names in the owner's logs (`rusted logs`) and a generic `missing_secrets` error to the caller. The same applies to mcp functions — tools read `context.env` exactly the same way.

Values are sealed with AES-256-GCM before they reach Postgres, under a key from the server's environment (`RUSTED_SECRETS_KEY`, 64 hex chars — `openssl rand -hex 32`). The database never holds a plaintext credential and the key never lives in the database, so neither alone reveals anything. A server without the key refuses to store secrets and says what to configure.

> Like `context.inbox`, `context.env` is absent under `rusted run` — local mode has no store to decrypt from, and says so at startup if the module requests secrets.

## Durable state

Functions are stateless between invocations — unless they ask:

```js
export const config = { state: true };

export default async function handler(request, context) {
  const counter = await context.state.get("hits");
  const wrote = await context.state.compareAndSet(
    "hits",
    counter?.version ?? null, // null = create; a version = replace exactly that
    (counter?.value ?? 0) + 1,
  );
  if (!wrote.ok) return context.json({ retry: true }, { status: 409 });
  return context.json({ hits: wrote.version });
}
```

`context.state` is durable JSON scoped to _(you, the function's name)_. It survives new revisions and even delete/redeploy — only the explicit purge (`rusted state purge <name>`, or the console's admin API) removes it, because a redeploy silently losing coordination state is a worse surprise than a few stale kilobytes.

Single-key compare-and-set is the whole transaction model: every entry carries a `version`, writes name the version they expect (`null` to create), and the check happens atomically in the database — two racers get exactly one winner and a `currentVersion` to retry from. Keys are 1–512 bytes, values up to 64 KiB serialized, `list` pages lexicographically 100 at a time, and your plan bounds total keys and bytes per function. There are no multi-key transactions; design state so one key is the unit of consistency.

Under `rusted run`, state is in-memory with identical semantics: it survives hot reloads and resets when the process exits.

## Object storage

For bytes too big for state — file contents, media, encrypted blobs — a function can declare a binding to an S3-compatible bucket (R2, S3, MinIO):

```js
export const config = {
  objects: {
    SHARES: {
      endpoint: "https://<account>.r2.cloudflarestorage.com",
      region: "auto",
      bucket: "renote-shares",
      maxObjectBytes: 67108864, // 64 MiB, enforced before signing
      accessKeyIdSecret: "R2_ACCESS_KEY_ID", // names in your secret vault —
      secretAccessKeySecret: "R2_SECRET_ACCESS_KEY", // the values never reach JS
    },
  },
};
```

`context.objects.SHARES` then hands out **presigned URLs** instead of moving bytes through the function: `presignPut(key, { contentLength, sha256 })` signs an upload for exactly those bytes — the provider itself rejects a different size, a different checksum, or an existing key (uploads are create-only) — and `presignGet` signs a short-lived download. `head`, `delete`, and `list` run host-side. URLs live 15–300 seconds.

The safety shape, since a binding is a credentialed HTTP client:

- Every key is silently prefixed with a namespace derived from you and the function — another function's objects are unreachable by construction, and `..`, control characters, and leading `/` are refused.
- Endpoints must be exact origins on the **server admin's allowlist** (`RUSTED_OBJECT_ENDPOINTS`, comma-separated); with no allowlist the capability is off. This is what keeps bindings from being an SSRF primitive.
- The credential secrets are resolved host-side per invocation. They are refused in `config.secrets`, so a module can use them for storage and never read them.
- Binding traffic is a host capability — it does not spend the invocation's outbound `fetch` allowance, though it stays inside the execution deadline.

Under `rusted run`, objects live in an isolated temp directory and the presigned URLs point at the dev server itself, with the same create-only/length/checksum enforcement — the flow you test locally is the flow that ships. [examples/state-and-objects](examples/state-and-objects) is a complete file using both.

## Environments

One deployed function, different configuration per environment — selected by the URL, never by code:

```
https://rusted.sh/f/settle           → prod (every existing URL, unchanged)
https://rusted.sh/f/@stage/settle    → stage
```

Environments are created in the console's Secrets page (`prod` always exists and cannot be deleted). Each is a full overlay: its own **secret values** for the same declared names, its own **durable state**, and its own **object namespace** — a stage invocation cannot read prod's counters or address prod's blobs, by construction. Handlers see `context.currentEnv` (`"prod"`, `"stage"`, … — always a string; `"local"` under `rusted run`), though the point is that most never need to look: the same `context.env.APP_ORIGIN` read resolves to whichever environment the URL selected.

The rules that keep it honest: a declared secret unset in the resolved environment refuses the invocation before the handler runs — never an empty string, never a silent fallback to prod's value. An environment the account never created answers 404 exactly like a missing function. Host-only secrets (object-binding credentials, `seal` keys, OAuth introspection credentials) resolve through the same environment. A push never changes which environments exist; deleting one removes its secrets after a confirm, while its durable state stays until purged.

Because the environment is part of the address, it survives everything code can't reach: register your dev GitHub OAuth app against `/f/@stage/renote-auth/callback` and the whole flow — redirects, cookies, third-party callbacks — stays in stage end to end. OAuth-protected mcp functions get per-environment resource identities: `/f/@stage/name` derives its audience by inserting the env segment into the declared one (`https://rusted.sh/f/name` → `https://rusted.sh/f/@stage/name`), publishes its own discovery metadata at the matching `/.well-known/oauth-protected-resource/f/@stage/name`, and validates tokens against exactly that audience — so a stage token never opens prod and vice versa, while introspection credentials resolve from the stage vault. (An audience with no `/f/` segment can't be derived; those functions refuse env URLs with `env_unsupported`.)

## Randomness

`Math.random()` is fine for jitter and dice; it is not fine for anything an attacker gains by predicting — and OAuth state, PKCE verifiers, session tokens, and encryption nonces are exactly that. The host lends the real thing instead:

```js
const bytes = context.randomBytes(32); // Uint8Array, straight from the OS CSPRNG
const state = context.randomBase64Url(32); // the same 256 bits as 43 URL- and cookie-safe chars
```

These draw from the operating system's cryptographic random source (`getrandom` on the host — the same pool `openssl rand` reads), not from the JavaScript engine. Lengths are 1 to 1024 bytes; anything else throws. Unlike `context.inbox` and `context.env`, randomness needs no owner to scope to, so it is present everywhere — `rusted run` included — and the same calls work in mcp tool handlers.

The independence matters as much as the unpredictability: mint a fresh value per purpose — one for the OAuth `state`, another for the PKCE verifier, another per encryption nonce — rather than deriving one from another.

Alongside it, the primitives credential handling always ends up needing, native rather than npm-imported: `context.sha256(data)` (string or bytes → `Uint8Array`), `context.toBase64Url` / `fromBase64Url` and `toHex` / `fromHex`, and `context.timingSafeEqual(a, b)` — compare tokens and signatures with that, never `===`; an interpreted "constant-time" loop isn't one. A PKCE challenge is now one line: `context.toBase64Url(context.sha256(verifier))`.

## Sealed values

Sessions, OAuth state, and anything else a function hands to a browser and must trust when it comes back wants authenticated encryption. `context.seal` does it host-side, keyed by one of your vault secrets:

```js
const cookie = await context.seal(
  { userId, expiresAt },
  { keySecret: "AUTH_COOKIE_KEY", context: "myapp:session:v1" },
);
// later, on any request:
const session = await context.open(request.cookies[NAME], {
  keySecret: "AUTH_COOKIE_KEY",
  context: "myapp:session:v1",
});
if (!session)
  return context.json({ error: "not_authenticated" }, { status: 401 });
```

The payload is any JSON value up to 16 KiB; the result is compact base64url, fit for a cookie. `open` answers the payload or `null` — tampering, a different key, and a different `context` string are all the same silent null, so a forger learns nothing about which check refused them. The `context` option is authenticated data: a value sealed for one purpose cannot be replayed into another.

Two properties worth noticing: the key secret does **not** need to appear in `config.secrets` — the module can seal with a key it can never read, which is strictly better than decrypting cookies with a key sitting in `context.env` — and the cipher is native AES-256-GCM instead of an interpreted JavaScript implementation burning your execution budget on every request. Under `rusted run`, sealing works against a per-process key: seals survive hot reloads and expire with the dev server.

For the HTTP chores around all this, the glue carries `request.cookies` (parsed), `context.redirect(url)`, `context.setCookie(name, value, options)` (secure, HttpOnly, SameSite=Lax by default), and `context.formEncode(values)` for query strings and form bodies.

## Using npm packages

The runtime executes exactly one file — `import` is rejected at push time. `rusted run` and `rusted build` bundle for you, so npm packages just work:

```bash
npm install yaml          # install what you import
rusted run index.js       # develop against it
rusted push index.js      # deploy it — bundled on the way
```

`rusted build index.js` writes `dist/index.js` if you want the artifact itself, for CI or inspection.

Bundling targets a neutral platform, so a dependency reaching for a Node builtin (`fs`, `http`, …) fails at build time rather than at runtime — those don't exist here. Pure-JS libraries work; keep an eye on bundle size, since source-size limits are part of the platform's design.

That includes crypto. There is no `node:crypto`, no `Buffer`, and no WebCrypto either — `crypto.subtle` is a browser/Node API this runtime doesn't have. Before reaching for a package, check what the host already lends: [`context.sha256`, base64url/hex codecs, `timingSafeEqual`](#randomness), [`context.seal`/`open`](#sealed-values) for authenticated encryption, and [`context.randomBytes`](#randomness) — together they cover cookies, PKCE, checksums, and token comparison with zero dependencies. For anything beyond that (HMAC variants, other curves), pick a pure-JS implementation such as [`@noble/hashes`](https://github.com/paulmillr/noble-hashes) rather than anything wrapping a platform API; it bundles in a few KB.

## Metrics

The server carries an in-process OpenTelemetry pipeline — no collector to run: every invocation increments `rusted.invocations` (by function and outcome) and executed handlers record `rusted.exec.duration`, a histogram of **pure handler execution time**. Read it back with your API key:

```bash
curl -H "Authorization: Bearer $RUSTED_API_KEY" https://rusted.sh/api/stats
```

```json
{
  "source": "opentelemetry",
  "functions": [
    {
      "function": "renote-auth",
      "invocations": 812,
      "success": 780,
      "error": 2,
      "terminated": 0,
      "refused": 30,
      "error_rate": 0.0025,
      "p95_exec_ms": 11.4
    }
  ]
}
```

`error_rate` counts only what a handler owns — errors and terminations over executed invocations; refusals (rate limits, wrong methods, unpublished) are tallied separately. `p95_exec_ms` is interpolated from histogram buckets tuned to the plans' execution budgets. The console dashboard's headline tiles read from this same pipeline (the day chart and invocation rows stay event-based from Postgres).

Counters are cumulative per process, so a background task folds the totals into Postgres every minute and a restarting server loads them back as its baseline — stats are cumulative per deployment, and a crash loses at most a minute of counts.

## License

Open source, split along the line between your machine and the service:

- **`rusted-engine` and `rusted-cli`** — Apache-2.0. Embed them, ship them, fork them.
- **`rusted-server`** — AGPL-3.0. Self-host it freely; publish your changes if you offer it to others over a network.

See [LICENSE.md](LICENSE.md) for the details and [CONTRIBUTING.md](CONTRIBUTING.md) to get set up. Contributions are accepted under the [DCO](https://developercertificate.org/) — `git commit -s`.
