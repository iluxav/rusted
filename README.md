# rusted

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
rusted invoke greet --body '{"name":"Bob"}'
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

### MCP functions

The other surface a file can export: tools instead of a request handler. The same push makes it a live MCP server at the same `/f/<name>` URL:

```js
export const mcp = {
  name: "slugger",
  tools: {
    slugify: {
      description: "Turn a title into a URL slug",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
      async handler({ text }) { return text.toLowerCase().replace(/[^a-z0-9]+/g, "-"); },
    },
  },
};
```

You write handlers; the platform speaks the protocol. `initialize` and `tools/list` are answered from deploy-time metadata, arguments are validated against each tool's `inputSchema` before any sandbox boots, and a thrown error comes back as an `isError` tool result the model can read and retry — never a protocol error. Return a string for text content; any other value is sent as JSON with `structuredContent`.

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

The endpoint demands an API key of the owner unless the file says `public: true`. A tool call is one invocation under your plan's limits; `initialize` and `tools/list` are free. A module exports `http` or `mcp`, never both — pushing one over the other switches the function's kind.

The dev loop is the http one: `rusted run index.js` serves the tools locally with hot reload and prints the same config block — connect a client, edit, push when it works. `rusted new my-tools --mcp` scaffolds a starting point, and [examples/mcp-server](examples/mcp-server) is a complete file.

This is distinct from the platform's own MCP server on the admin port (`/mcp`), whose tools — execute, deploy, list, delete, inbox_create, inbox_read — an agent uses to build on rusted. An mcp *function* is what such an agent (or you) deploys: it serves the tools in the file, under the owner's key and limits.

## Signing in

```bash
rusted login
```

Prints a short code, you approve it once in a browser, and the CLI stores the key it's granted in `~/.config/rusted/credentials.json` (owner-readable only). Nothing to copy, and the key it mints appears in the console's **API keys** page like any other — revoke it there if you lose the machine.

Resolution order is `--api-key` → `RUSTED_API_KEY` → that file, so CI keeps using an environment variable and needs no browser.

## Console, auth, and API keys

The web console lives on the admin port (http://127.0.0.1:7412). Sign-in is real GitHub OAuth: create an OAuth app at github.com/settings/developers with callback `http://127.0.0.1:7412/auth/github/callback`, then start the server with `GITHUB_CLIENT_ID` and `GITHUB_CLIENT_SECRET` set (the login page shows these instructions until you do).

API keys are minted in the console (shown once; only a hash is stored). `rusted serve --require-auth` makes every function endpoint demand `Authorization: Bearer rk_live_…`. Key verification is served from an in-memory cache invalidated over Postgres LISTEN/NOTIFY — revocation propagates in milliseconds without per-request DB reads.

Everything lives in Postgres (`DATABASE_URL`, default `postgres://rusted:rusted@127.0.0.1:5457/rusted`; connect directly with `psql` on port 5457).

Configuration comes from the environment, and `rusted` loads a `.env` from the working directory at startup — `cp .env.example .env`, fill in the GitHub credentials, done. `.env` is gitignored.

Every command takes `--json` for stable machine-readable output.

## What's here

| Path                   | Purpose                                                                                                                                                                      |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/rusted-engine` | QuickJS executor: uncatchable wall-clock interrupt, heap cap, output cap, structured `console` logs, fresh context per invocation                                            |
| `crates/rusted-server` | Orchestrator: data API (`/f/<name>`, `/r/<id>`), admin API for the CLI, content-addressed Postgres store with immutable revisions, inboxes, per-function concurrency 1, temp-run TTLs |
| `crates/rusted-cli`    | The single `rusted` binary                                                                                                                                                   |

The engine choice (QuickJS over Boa) came out of a measured spike — cold start, throughput, memory, and whether hostile code can actually be stopped. QuickJS won on all four, decisively on the last: it can interrupt a runaway script uncatchably and cap its heap, which Boa cannot.

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

### Limits while developing

Local runs get the most permissive plan's limits — 30s execution, 25 outbound calls — so nothing blocks you mid-thought. Each run then reports what it _would_ cost:

```
✓ 200 POST /convert/1   wall 1024ms · exec 1002ms
  ⚠ needs Extra — 1000ms exec over 50ms on Dev
```

With `RUSTED_API_KEY` set, rusted looks up your actual plan in the background — never blocking startup — and the warning names it directly: `over your Pro plan: 1000ms exec over 500ms`.

Useful flags for `run`: `--port`, `--exec-ms` (execution budget), `--outbound` (fetch calls allowed per invocation), and `--build 'your command'` to replace the built-in bundling with your own pipeline. To develop against a specific tier, set both: `--exec-ms 100 --outbound 2` is the Dev plan.

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

A function can call out. Nothing can call *in* — which rules out OAuth callbacks, webhooks, form submissions, and anything a third party has to initiate. That's especially true of an agent running in a browser or someone's cloud: it has no address at all.

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

## Using npm packages

The runtime executes exactly one file — `import` is rejected at push time. `rusted run` and `rusted build` bundle for you, so npm packages just work:

```bash
npm install yaml          # install what you import
rusted run index.js       # develop against it
rusted push index.js      # deploy it — bundled on the way
```

`rusted build index.js` writes `dist/index.js` if you want the artifact itself, for CI or inspection.

Bundling targets a neutral platform, so a dependency reaching for a Node builtin (`fs`, `http`, …) fails at build time rather than at runtime — those don't exist here. Pure-JS libraries work; keep an eye on bundle size, since source-size limits are part of the platform's design.

## License

Open source, split along the line between your machine and the service:

- **`rusted-engine` and `rusted-cli`** — Apache-2.0. Embed them, ship them, fork them.
- **`rusted-server`** — AGPL-3.0. Self-host it freely; publish your changes if you offer it to others over a network.

See [LICENSE.md](LICENSE.md) for the details and [CONTRIBUTING.md](CONTRIBUTING.md) to get set up. Contributions are accepted under the [DCO](https://developercertificate.org/) — `git commit -s`.
