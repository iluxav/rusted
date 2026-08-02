# rusted

A microfunction platform where a tiny JavaScript file becomes a live HTTP endpoint in seconds, executed by QuickJS inside a restricted Rust runtime.

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
```

### Deploying

```bash
cat > greet.js <<'EOF'
export default async function handler(request, context) {
  const input = await request.json();
  return context.json({ message: `Hello, ${input.name}` });
}
EOF

rusted push greet.js --name greet        # → http://127.0.0.1:7411/f/greet
curl -X POST http://127.0.0.1:7411/f/greet -d '{"name":"Ada"}'
# {"message":"Hello, Ada"}

rusted preview greet.js --ttl 120        # temporary endpoint, expires on its own
rusted invoke greet --body '{"name":"Bob"}'
rusted logs greet                        # recent invocations with console output
rusted list | rusted pull greet | rusted verify greet.js | rusted delete greet
```

Scripts can carry their own deployment intent, so `rusted push api.js` needs no flags:

```js
export const config = {
  name: "api",
  methods: ["GET", "POST"],
  path: "/users/{id}",
};

export default async function handler(request, context) { ... }
```

Explicit flags override the file config. Unknown config keys fail verify — typos can't deploy silently. Being real code (not comments), the config survives bundling.

## Console, auth, and API keys

The web console lives on the admin port (http://127.0.0.1:7412). Sign-in is real GitHub OAuth: create an OAuth app at github.com/settings/developers with callback `http://127.0.0.1:7412/auth/github/callback`, then start the server with `GITHUB_CLIENT_ID` and `GITHUB_CLIENT_SECRET` set (the login page shows these instructions until you do).

API keys are minted in the console (shown once; only a hash is stored). `rusted serve --require-auth` makes every function endpoint demand `Authorization: Bearer rk_live_…`. Key verification is served from an in-memory cache invalidated over Postgres LISTEN/NOTIFY — revocation propagates in milliseconds without per-request DB reads.

Everything lives in Postgres (`DATABASE_URL`, default `postgres://rusted:rusted@127.0.0.1:5457/rusted`; connect directly with `psql` on port 5457).

Configuration comes from the environment, and `rusted` loads a `.env` from the working directory at startup — `cp .env.example .env`, fill in the GitHub credentials, done. `.env` is gitignored.

Every command takes `--json` for stable machine-readable output.

## What's here

| Path | Purpose |
|---|---|
| `crates/rusted-engine` | QuickJS executor: uncatchable wall-clock interrupt, heap cap, output cap, structured `console` logs, fresh context per invocation |
| `crates/rusted-server` | Orchestrator: data API (`/f/<name>`, `/r/<id>`), admin API for the CLI, content-addressed Postgres store with immutable revisions, per-function concurrency 1, temp-run TTLs |
| `crates/rusted-cli` | The single `rusted` binary |

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

Local runs get the most permissive plan's limits — 30s execution, 25 outbound calls — so nothing blocks you mid-thought. Each run then reports what it *would* cost:

```
✓ 200 POST /convert/1   wall 1024ms · exec 1002ms
  ⚠ needs Extra — 1000ms exec over 50ms on Dev
```

With `RUSTED_API_KEY` set, rusted looks up your actual plan in the background — never blocking startup — and the warning names it directly: `over your Pro plan: 1000ms exec over 500ms`.

Useful flags for `run`: `--port`, `--exec-ms` (execution budget), `--outbound` (fetch calls allowed per invocation), and `--build 'your command'` to replace the built-in bundling with your own pipeline. To develop against a specific tier, set both: `--exec-ms 100 --outbound 2` is the Dev plan.

## Using npm packages

The runtime executes exactly one file — `import` is rejected at push time. `rusted run` and `rusted build` bundle for you, so npm packages just work:

```bash
npm install yaml          # install what you import
rusted run index.js       # develop against it
rusted build index.js     # → dist/index.js
rusted push dist/index.js --name my-fn
```

Bundling targets a neutral platform, so a dependency reaching for a Node builtin (`fs`, `http`, …) fails at build time rather than at runtime — those don't exist here. Pure-JS libraries work; keep an eye on bundle size, since source-size limits are part of the platform's design.

## License

Open source, split along the line between your machine and the service:

- **`rusted-engine` and `rusted-cli`** — Apache-2.0. Embed them, ship them, fork them.
- **`rusted-server`** — AGPL-3.0. Self-host it freely; publish your changes if you offer it to others over a network.

See [LICENSE.md](LICENSE.md) for the details and [CONTRIBUTING.md](CONTRIBUTING.md) to get set up. Contributions are accepted under the [DCO](https://developercertificate.org/) — `git commit -s`.
