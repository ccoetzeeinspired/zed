import net from "node:net";

const DEFAULT_PORT = 19382;

export type IpcResponse = {
  id: unknown;
  ok: boolean;
  result?: unknown;
  error?: string;
};

function ipcPort(): number {
  const raw = process.env.ZED_BROWSER_AUTOMATION_PORT;
  if (!raw) {
    return DEFAULT_PORT;
  }
  const port = Number.parseInt(raw, 10);
  return Number.isFinite(port) ? port : DEFAULT_PORT;
}

/** Call the Zed browser automation loopback IPC (one JSON line in, one out). */
export function callZedAutomation(
  method: string,
  params: Record<string, unknown> = {},
): Promise<IpcResponse> {
  const id = crypto.randomUUID();
  const payload = JSON.stringify({ id, method, params });

  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: "127.0.0.1", port: ipcPort() });
    let buffer = "";

    const fail = (err: Error) => {
      socket.destroy();
      reject(err);
    };

    socket.setTimeout(120_000, () => fail(new Error("Zed browser automation IPC timed out")));
    socket.on("error", fail);

    socket.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      const newline = buffer.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const line = buffer.slice(0, newline).trim();
      socket.end();
      try {
        resolve(JSON.parse(line) as IpcResponse);
      } catch (err) {
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });

    socket.on("connect", () => {
      socket.write(`${payload}\n`);
    });
  });
}

export async function requireZedOk(response: IpcResponse): Promise<unknown> {
  if (!response.ok) {
    throw new Error(
      response.error ??
        "Zed browser automation failed — is Zed running with an embedded browser tab open?",
    );
  }
  return response.result;
}
