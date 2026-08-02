# MCP server

An [MCP](https://modelcontextprotocol.io) tool server in one file, with no
dependencies.

```bash
rusted run index.js
# POST http://127.0.0.1:7400/f/mcp
```

Point an MCP client at that URL, or drive it by hand:

```bash
curl -X POST http://127.0.0.1:7400/f/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Or connect a real client:

```bash
claude mcp add --transport http rusted-demo http://127.0.0.1:7400/f/mcp
npx @modelcontextprotocol/inspector --cli http://127.0.0.1:7400/f/mcp --transport http --method tools/list
```

## Why this works

MCP's Streamable HTTP transport lets a server answer a POST with a single
`application/json` JSON-RPC response instead of opening an SSE stream. That is
exactly the shape of a rusted function: one request in, one response out. A
stateless tool server needs nothing more.

`initialize`, `ping`, `tools/list`, and `tools/call` are all request/response,
so the whole useful surface of a tool provider fits. Tool failures come back as
results with `isError: true` rather than protocol errors, which is what lets a
model see what went wrong and try again.

This was checked against the official SDK client rather than assumed: the MCP
inspector connects, negotiates `initialize`, lists these tools, and calls them,
with no stream involved. A client's `GET` for the optional server-initiated
stream gets a `405`, which is what the spec prescribes for a server that
doesn't offer one — so nothing waits on a stream that will never open.

## What doesn't work yet

- **Server-initiated messages.** No SSE, so no progress notifications, no
  `tools/list_changed`, and no `GET` stream. Requests the client makes are
  answered; the server can't speak first. A client's `GET` gets a `405`, which
  is what the spec prescribes for a server that offers no stream.
- **Real sessions.** The `Mcp-Session-Id` header is echoed rather than backed by
  stored state, because this function keeps none. Enough for clients that expect
  the header; not enough for per-client state.

Everything else the transport asks of a stateless server is here: `202` with no
body for notifications, `400` for malformed JSON, and a session header on
replies — all through `context.json(body, { status, headers })`.

## Keep in mind

- **Execution budget.** These tools run in ~0.05 ms, well inside the Dev plan's
  100 ms. A tool that calls an API needs `fetch`, which the free tier allows
  (2 calls per invocation).
- **Concurrency.** Agents fan tool calls out in parallel; each plan sets how
  many run at once (Dev 2, Pro 5, Extra 20). Beyond that, calls queue and then
  get a `busy` response.
