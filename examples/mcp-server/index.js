export const http = {
  name: "mcp",
  methods: ["POST"],
  path: "/",
};

// A stateless MCP server over Streamable HTTP. The spec lets a server answer
// a POST with a single `application/json` JSON-RPC response instead of an SSE
// stream, which is exactly what a rusted function can do — so tool servers
// work without sessions or streaming.

const TOOLS = [
  {
    name: "slugify",
    description: "Turn a title into a URL slug.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string", description: "The text to slugify" } },
      required: ["text"],
    },
  },
  {
    name: "word_count",
    description: "Count words, characters, and lines in a passage of text.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
  },
];

function call(name, args) {
  switch (name) {
    case "slugify":
      return String(args.text ?? "")
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-+|-+$/g, "");
    case "word_count": {
      const text = String(args.text ?? "");
      const words = text.trim() ? text.trim().split(/\s+/).length : 0;
      return JSON.stringify({ words, characters: text.length, lines: text.split("\n").length });
    }
    default:
      throw new Error(`unknown tool: ${name}`);
  }
}

function handle(message) {
  const { id, method, params } = message;
  // Notifications carry no id and expect no reply.
  if (id === undefined) return null;

  const ok = (result) => ({ jsonrpc: "2.0", id, result });
  const fail = (code, message) => ({ jsonrpc: "2.0", id, error: { code, message } });

  switch (method) {
    case "initialize":
      return ok({
        protocolVersion: params?.protocolVersion ?? "2025-06-18",
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "rusted-example-mcp", version: "1.0.0" },
      });
    case "ping":
      return ok({});
    case "tools/list":
      return ok({ tools: TOOLS });
    case "tools/call":
      try {
        return ok({
          content: [{ type: "text", text: call(params?.name, params?.arguments ?? {}) }],
        });
      } catch (e) {
        // Tool failures are results, not protocol errors — that's how the
        // model gets to see what went wrong and try again.
        return ok({ content: [{ type: "text", text: e.message }], isError: true });
      }
    default:
      return fail(-32601, `method not found: ${method}`);
  }
}

export default async function handler(request, context) {
  let message;
  try {
    message = await request.json();
  } catch {
    return context.json(
      { jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } },
      { status: 400 },
    );
  }

  // JSON-RPC allows a batch; answer only the messages that asked for a reply.
  const replies = Array.isArray(message)
    ? message.map(handle).filter(Boolean)
    : handle(message);

  // Nothing asked for an answer, so say so the way the spec does.
  if (replies === null || (Array.isArray(replies) && replies.length === 0)) {
    return context.text("", { status: 202 });
  }

  // A session id lets a client correlate its requests. This server keeps no
  // state, so the id is derived rather than stored — enough to satisfy clients
  // that expect one, honest about there being nothing behind it.
  const session = request.headers["mcp-session-id"] || "stateless";
  return context.json(replies, { headers: { "mcp-session-id": session } });
}
