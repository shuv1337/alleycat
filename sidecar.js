import { drizzle } from "drizzle-orm/node-sqlite/driver";
import * as http2 from "node:http";
import * as tls from "node:tls";
const parentPort = getParentPort();
let listener;
parentPort.on("message", (event) => {
  const command = parseCommand(event.data);
  if (!command) return;
  if (command.type === "stop") {
    void stop();
    return;
  }
  void start(command);
});
async function start(command) {
  try {
    prepareSidecarEnv(command.password, command.userDataPath);
    ensureLoopbackNoProxy();
    useSystemCertificates();
    useEnvProxy();
    const { Database, JsonMigration, Log, Server } = await import("./chunks/node-Cgkzg4Bc.js");
    await Log.init({ level: "WARN" });
    if (command.needsMigration) {
      await JsonMigration.run(drizzle({ client: Database.Client().$client }), {
        progress: (event) => {
          parentPort.postMessage({
            type: "sqlite",
            progress: {
              type: "InProgress",
              value: event.total === 0 ? 100 : Math.round(event.current / event.total * 100)
            }
          });
        }
      });
      parentPort.postMessage({ type: "sqlite", progress: { type: "Done" } });
    }
    listener = await Server.listen({
      port: command.port,
      hostname: command.hostname,
      username: "opencode",
      password: command.password,
      cors: ["oc://renderer"]
    });
    parentPort.postMessage({ type: "ready" });
  } catch (error) {
    parentPort.postMessage({ type: "error", error: serializeError(error) });
    setImmediate(() => process.exit(1));
  }
}
async function stop() {
  try {
    await listener?.stop();
  } finally {
    listener = void 0;
    parentPort.postMessage({ type: "stopped" });
    setImmediate(() => process.exit(0));
  }
}
function prepareSidecarEnv(password, userDataPath) {
  Object.assign(process.env, {
    OPENCODE_SERVER_USERNAME: "opencode",
    OPENCODE_SERVER_PASSWORD: password,
    XDG_STATE_HOME: process.env.XDG_STATE_HOME ?? userDataPath
  });
}
function ensureLoopbackNoProxy() {
  const loopback = ["127.0.0.1", "localhost", "::1"];
  const upsert = (key) => {
    const items = (process.env[key] ?? "").split(",").map((value) => value.trim()).filter((value) => Boolean(value));
    for (const host of loopback) {
      if (items.some((value) => value.toLowerCase() === host)) continue;
      items.push(host);
    }
    process.env[key] = items.join(",");
  };
  upsert("NO_PROXY");
  upsert("no_proxy");
}
function useSystemCertificates() {
  try {
    const nodeTls = tls;
    nodeTls.setDefaultCACertificates([
      .../* @__PURE__ */ new Set([...nodeTls.getCACertificates("default"), ...nodeTls.getCACertificates("system")])
    ]);
  } catch (error) {
    console.warn("failed to load system certificates", error);
  }
}
function useEnvProxy() {
  try {
    ;
    http2.setGlobalProxyFromEnv();
  } catch (error) {
    console.warn("failed to load proxy environment", error);
  }
}
function parseCommand(value) {
  if (!value || typeof value !== "object") return;
  const command = value;
  if (command.type === "stop") return { type: "stop" };
  if (command.type !== "start") return;
  if (typeof command.hostname !== "string") return;
  if (typeof command.port !== "number") return;
  if (typeof command.password !== "string") return;
  if (typeof command.userDataPath !== "string") return;
  if (typeof command.needsMigration !== "boolean") return;
  return {
    type: "start",
    hostname: command.hostname,
    port: command.port,
    password: command.password,
    userDataPath: command.userDataPath,
    needsMigration: command.needsMigration
  };
}
function serializeError(error) {
  if (error instanceof Error) return { message: error.message, stack: error.stack };
  return { message: String(error) };
}
function getParentPort() {
  const port = process.parentPort;
  if (!port) throw new Error("Sidecar parent port unavailable");
  return port;
}
