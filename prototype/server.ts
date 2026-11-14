// Serves the workshop page and bridges it to the installed `leds` CLI, so the
// page shows exactly what the service renders and can preview on the device.

import index from "./index.html";

async function leds(args: string[], stdin = ""): Promise<string> {
  const child = Bun.spawn(["leds", ...args], { stdin: new Blob([stdin]), stdout: "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (code !== 0) throw new Error(err.replace(/^Error: /, "").trim() || `leds exited with ${code}`);
  return out;
}

/** Wraps a handler so a failed CLI call reaches the page as a readable message. */
function handler(run: (body: any) => Promise<unknown>) {
  return async (request: Request) => {
    try {
      const body = request.method === "POST" ? await request.json() : undefined;
      return Response.json((await run(body)) ?? null);
    } catch (error) {
      return Response.json({ error: error instanceof Error ? error.message : String(error) }, { status: 400 });
    }
  };
}

const server = Bun.serve({
  hostname: "127.0.0.1",
  port: Number(process.env.PORT) || 4477,
  development: true,
  routes: {
    "/": index,
    "/sdled.wasm": () => new Response(Bun.file(`${import.meta.dir}/vendor/sdled.wasm`)),
    "/api/palettes": handler(async () => JSON.parse(await leds(["palette", "--json"]))),
    "/api/render": { POST: handler(async (scene) => JSON.parse(await leds(["render"], JSON.stringify(scene)))) },
    "/api/play": {
      POST: handler(async ({ program, seconds }) => {
        await leds(["play", "-", "--seconds", String(seconds)], program);
      }),
    },
    "/api/config": {
      POST: handler(async ({ colors, brightness, front_led }) => {
        for (const [state, color] of Object.entries(colors)) await leds(["color", state, String(color)]);
        await leds(["brightness", String(brightness)]);
        await leds(["front", String(front_led)]);
      }),
    },
  },
});

console.log(`leds workshop: ${server.url}`);
