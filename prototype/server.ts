// Serves the workshop page and bridges it to the installed `leds` CLI, so the
// page shows exactly what the service renders, can preview on the device and
// can save its settings to the service.

import index from "./index.html";

// The dev server watches for changes under the directory it runs in, so run
// in this one wherever the command was started from.
process.chdir(import.meta.dir);

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
    // Renders a scene as the service would.
    "/api/render": { POST: handler(async (scene) => JSON.parse(await leds(["render"], JSON.stringify(scene)))) },
    // Shows a scene on the device for a while.
    "/api/show": {
      POST: handler(async ({ scene, seconds }) => {
        await leds(["show", "--seconds", String(seconds)], JSON.stringify(scene));
      }),
    },
    // Shows a raw program on the device for a while.
    "/api/play": {
      POST: handler(async ({ program, seconds }) => {
        await leds(["play", "-", "--seconds", String(seconds)], program);
      }),
    },
    // Saves every setting to the service and returns what it saved.
    "/api/config": { POST: handler(async (config) => JSON.parse(await leds(["configure"], JSON.stringify(config)))) },
  },
});

console.log(`leds workshop: ${server.url}`);
