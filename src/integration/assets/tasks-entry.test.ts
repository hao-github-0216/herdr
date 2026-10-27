import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Fresh hosts only: never load user settings, credentials, sessions or transcripts.
const publicPiAiModule = process.env.HERDR_TEST_PI_AI_MODULE;

async function freshPiHost(publicSdk = false) {
  const dir = await mkdtemp(join(tmpdir(), "herdr-withdrawn-pi-"));
  const binary = join(dir, "fixture-herdr");
  const calls = join(dir, "unexpected-calls");
  await writeFile(binary, `#!/bin/sh\nprintf 'unexpected probe' >> '${calls}'\n`, { mode: 0o700 });
  const keys = ["HOME", "HERDR_ENV", "HERDR_BIN_PATH", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "HERDR_WORKSPACE_ID", "OMPCODE"];
  const saved = Object.fromEntries(keys.map(key => [key, process.env[key]]));
  try {
    Object.assign(process.env, { HOME: dir, HERDR_ENV: "1", HERDR_BIN_PATH: binary,
      HERDR_SOCKET_PATH: join(dir, "unused.sock"), HERDR_PANE_ID: "pane_fixture", HERDR_WORKSPACE_ID: "ws_fixture" });
    delete process.env.OMPCODE;
    if (publicSdk) {
      // Load the explicitly supplied public artifact, never Pi's private config.
      const { Type } = await import(publicPiAiModule!);
      expect(Type).toBeDefined();
    }
    const tools: unknown[] = [];
    const handlers = new Map<string, any>();
    const { default: extension } = await import(`./pi/herdr-agent-state.ts?withdrawn=${Date.now()}-${publicSdk}`);
    await extension({ registerTool: (tool: unknown) => tools.push(tool),
      on: (name: string, handler: any) => handlers.set(name, handler), events: { on() {} } });
    expect(tools).toEqual([]);
    expect(handlers.has("before_agent_start")).toBe(false);
    for (const mode of ["rpc", "json", "print"]) {
      await handlers.get("session_start")({ reason: "startup" }, { mode });
      await handlers.get("model_select")({ model: { id: "headless" } }, { mode });
    }
    expect(handlers.has("session_start")).toBe(true);
    expect(handlers.has("model_select")).toBe(true);
    expect(handlers.has("agent_start")).toBe(true);
    expect(handlers.has("agent_end")).toBe(false);
    expect(await Bun.file(calls).exists()).toBe(false);
  } finally {
    for (const key of keys) {
      if (saved[key] === undefined) delete process.env[key]; else process.env[key] = saved[key];
    }
    await rm(dir, { recursive: true, force: true });
  }
}

test("fresh Pi registers zero Tasks tools or prompt injections, including headless hosts", () => freshPiHost());
test.skipIf(!publicPiAiModule)("fresh Pi with public SDK retains reporters without Tasks entry", () => freshPiHost(true));

test("fresh native Node Pi host has no Tasks registration, guidance or binary probes", async () => {
  const dir = await mkdtemp(join(tmpdir(), "herdr-withdrawn-native-pi-"));
  const script = join(dir, "host.mjs");
  const binary = join(dir, "fixture-herdr");
  const calls = join(dir, "unexpected-calls");
  await writeFile(binary, `#!/bin/sh\nprintf 'unexpected probe' >> '${calls}'\n`, { mode: 0o700 });
  await writeFile(script, `
    import extension from ${JSON.stringify(join(import.meta.dir, "pi/herdr-agent-state.ts"))};
    const tools = [], handlers = new Map();
    await extension({registerTool: t => tools.push(t), on: (n,h) => handlers.set(n,h), events:{on(){}}});
    for (const mode of ["rpc", "json", "print"]) await handlers.get("session_start")({reason:"startup"},{mode});
    console.log(JSON.stringify({tools, injection:handlers.has("before_agent_start"), model:handlers.has("model_select"), state:handlers.has("agent_start")}));
  `);
  try {
    const child = Bun.spawn(["node", "--experimental-strip-types", script], {
      env: { ...process.env, HOME: dir, HERDR_ENV: "1", OMPCODE: "", HERDR_BIN_PATH: binary,
        HERDR_SOCKET_PATH: join(dir, "unused.sock"), HERDR_PANE_ID: "pane_fixture", HERDR_WORKSPACE_ID: "ws_fixture" },
      stdout: "pipe", stderr: "pipe",
    });
    const output = await new Response(child.stdout).text();
    const stderr = await new Response(child.stderr).text();
    expect(await child.exited, stderr).toBe(0);
    expect(JSON.parse(output)).toEqual({ tools: [], injection: false, model: true, state: true });
    expect(await Bun.file(calls).exists()).toBe(false);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("Claude SessionStart emits no Tasks context or adapter probes; model/state reporters remain", async () => {
  const dir = await mkdtemp(join(tmpdir(), "herdr-withdrawn-claude-"));
  const binary = join(dir, "fixture-herdr");
  const calls = join(dir, "unexpected-calls");
  await writeFile(binary, `#!/bin/sh\nprintf 'unexpected probe' >> '${calls}'\n`, { mode: 0o700 });
  try {
    const env = { ...process.env, HOME: dir, HERDR_ENV: "1", HERDR_BIN_PATH: binary,
      HERDR_SOCKET_PATH: join(dir, "unused.sock"), HERDR_PANE_ID: "pane_fixture", HERDR_WORKSPACE_ID: "ws_fixture" };
    delete env.CURSOR_VERSION;
    for (const event of ["SessionStart", "Stop"]) {
      const child = Bun.spawn(["sh", join(import.meta.dir, "claude/herdr-agent-state.sh"), "session"], {
        env,
        stdin: "pipe", stdout: "pipe", stderr: "pipe",
      });
      child.stdin.write(JSON.stringify({ hook_event_name: event, session_id: "synthetic", model: "selected-model",
        transcript_path: join(dir, "never-read-transcript") }));
      child.stdin.end();
      expect(await new Response(child.stdout).text()).toBe("");
      expect(await child.exited).toBe(0);
    }
    expect(await Bun.file(calls).exists()).toBe(false);
    for (const asset of ["claude/herdr-agent-state.sh", "claude/herdr-agent-state.ps1", "pi/herdr-agent-state.ts"]) {
      const source = await readFile(join(import.meta.dir, asset), "utf8");
      expect(source).not.toMatch(/task adapter|herdr_task_|herdr_tasks|before_agent_start|registerTaskTools|additionalContext/);
      expect(source).toContain("model_agent");
      expect(source).toContain(asset.startsWith("pi/") ? "publishModel" : (asset.endsWith(".sh") ? "pane.report_metadata" : "report-metadata"));
    }
  } finally { await rm(dir, { recursive: true, force: true }); }
});
