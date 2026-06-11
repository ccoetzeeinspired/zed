export type IpcResponse = {
    id: unknown;
    ok: boolean;
    result?: unknown;
    error?: string;
};
/** Call the Zed browser automation loopback IPC (one JSON line in, one out). */
export declare function callZedAutomation(method: string, params?: Record<string, unknown>): Promise<IpcResponse>;
export declare function requireZedOk(response: IpcResponse): Promise<unknown>;
