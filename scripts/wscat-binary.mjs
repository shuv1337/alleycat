#!/usr/bin/env node
import { Buffer } from "node:buffer";

const url = process.argv[2];
const payload = process.argv[3] ? JSON.parse(process.argv[3]) : JSON.parse(await readStdin());
if (!url) {
  console.error("usage: scripts/wscat-binary.mjs <ws-url> '<json-payload>'");
  process.exit(2);
}

const body = Buffer.from(JSON.stringify(payload));
const frame = Buffer.alloc(4 + body.length);
frame.writeUInt32BE(body.length, 0);
body.copy(frame, 4);

const ws = new WebSocket(url);
const timeout = setTimeout(() => {
  console.error("timed out waiting for websocket response");
  ws.close();
  process.exit(1);
}, 5000);

ws.addEventListener("open", () => ws.send(frame));
ws.addEventListener("message", async (event) => {
  clearTimeout(timeout);
  const bytes = Buffer.from(await event.data.arrayBuffer());
  const length = bytes.readUInt32BE(0);
  const response = JSON.parse(bytes.subarray(4, 4 + length).toString("utf8"));
  console.log(JSON.stringify(response, null, 2));
  ws.close();
  process.exit(0);
});
ws.addEventListener("error", () => {
  clearTimeout(timeout);
  console.error("websocket error");
  process.exit(1);
});

function readStdin() {
  return new Promise((resolve, reject) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => (data += chunk));
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", reject);
  });
}
