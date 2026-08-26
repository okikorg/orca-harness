#!/usr/bin/env bun
/** A dependency-free static server for the external Orcacode manual. */
const root = new URL("./", import.meta.url);
const port = Number(Bun.env.PORT ?? 3030);
const mime: Record<string, string> = {
  ".html": "text/html; charset=utf-8", ".css": "text/css; charset=utf-8",
  ".js": "application/javascript; charset=utf-8", ".svg": "image/svg+xml",
  ".json": "application/json; charset=utf-8", ".ico": "image/x-icon",
};

const server = Bun.serve({
  port,
  async fetch(request) {
    const url = new URL(request.url);
    const pathname = url.pathname === "/" ? "/index.html" : url.pathname;
    if (pathname.includes("..")) return new Response("Not found", { status: 404 });
    const file = Bun.file(new URL(`.${pathname}`, root));
    if (!(await file.exists())) return new Response("Not found", { status: 404 });
    const ext = pathname.slice(pathname.lastIndexOf("."));
    return new Response(file, { headers: { "content-type": mime[ext] ?? "application/octet-stream", "cache-control": "no-cache" } });
  },
});

console.log(`Orcacode manual: http://localhost:${server.port}`);
