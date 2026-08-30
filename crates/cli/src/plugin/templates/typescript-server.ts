import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { pathToFileURL } from "node:url";
import { z } from "zod";

export function echo(text: string): string {
  return text;
}

export function createServer(): McpServer {
  const server = new McpServer({ name: "{{NAME}}", version: "0.1.0" });
  server.registerTool(
    "echo",
    { description: "Return the supplied text", inputSchema: { text: z.string() } },
    async ({ text }) => ({ content: [{ type: "text", text: echo(text) }] }),
  );
  return server;
}

async function main(): Promise<void> {
  await createServer().connect(new StdioServerTransport());
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
