// An MCP tool server in one file. The `mcp` export is the whole server:
// the platform speaks the protocol (initialize, tools/list, tools/call),
// validates arguments against each inputSchema, and runs only the handlers.
// A string result becomes text content; an object becomes structured content.

export const mcp = {
  name: "mcp",
  tools: {
    slugify: {
      description: "Turn a title into a URL slug.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string", description: "The text to slugify" } },
        required: ["text"],
      },
      async handler({ text }) {
        return text.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
      },
    },
    word_count: {
      description: "Count words, characters, and lines in a passage of text.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
      async handler({ text }) {
        const words = text.trim() ? text.trim().split(/\s+/).length : 0;
        return { words, characters: text.length, lines: text.split("\n").length };
      },
    },
  },
};
