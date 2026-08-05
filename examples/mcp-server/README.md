# MCP server

An [MCP](https://modelcontextprotocol.io) tool server in one file, with no
dependencies. The `mcp` export declares the tools; the platform speaks the
protocol, so the file is nothing but handlers.

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

When it works, `rusted push index.js` deploys it and prints the config block
to paste into a client. The deployed endpoint requires your API key unless
the file says `public: true`.

## What the platform handles

MCP's Streamable HTTP transport lets a server answer a POST with a single
`application/json` JSON-RPC response instead of opening an SSE stream — which
is exactly the shape of a rusted function. The platform serves `initialize`,
`ping`, and `tools/list` from deploy-time metadata, validates `tools/call`
arguments against the tool's `inputSchema` before any sandbox boots, and
turns a thrown error into an `isError: true` result the calling model can
read and retry — never a protocol error. A handler returning a string sends
text content; any other value is sent as JSON and mirrored in
`structuredContent`, which is what `word_count` does here.

This file used to hand-roll all of that — ~110 lines of JSON-RPC dispatch
over the http surface. Git history has the before; the tools are unchanged.

## Keep in mind

- **What counts.** A tool call is one invocation under your plan's limits;
  `initialize` and `tools/list` are metadata reads and cost nothing.
- **Execution budget.** These tools run in ~0.05 ms, well inside the Dev plan's
  100 ms. A tool that calls an API needs `fetch`, which the free tier allows
  (2 calls per invocation).
- **Concurrency.** Agents fan tool calls out in parallel; each plan sets how
  many run at once (Dev 2, Pro 5, Extra 20). Beyond that, calls queue and then
  get a `busy` response.
