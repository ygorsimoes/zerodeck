export const MessageType = {
  Error: -1,
  Call: 0,
  Reply: 1,
  Discard: 2,
  ReceivedResponse: 3,
  FullSync: 4,
  Event: 5,
} as const;

export type MessageType = (typeof MessageType)[keyof typeof MessageType];

export interface CallMessage {
  type: typeof MessageType.Call;
  args: unknown[];
  route: string;
  id: number;
}

export interface ReplyMessage {
  type: typeof MessageType.Reply;
  result: unknown;
  id: number;
}

export interface DiscardMessage {
  type: typeof MessageType.Discard;
  id: number;
}

export interface ReceivedResponseMessage {
  type: typeof MessageType.ReceivedResponse;
  id: number;
}

export interface FullSyncMessage {
  type: typeof MessageType.FullSync;
  messages: CallMessage[];
}

export interface ErrorMessage {
  type: typeof MessageType.Error;
  error: { name: string; error: string; traceback: string | null };
  id: number;
}

export interface EventMessage {
  type: typeof MessageType.Event;
  event: string;
  args: unknown[];
}

type MessageToBackend = CallMessage | ReceivedResponseMessage | FullSyncMessage;
type MessageFromBackend = ReplyMessage | ErrorMessage | DiscardMessage | EventMessage;

export interface WebSocketLike {
  readonly readyState: number;
  addEventListener(type: string, listener: (event: unknown) => void): void;
  send(data: string): void;
}

export interface WSRouterOptions {
  baseUrl?: string;
  /** Delay before the first reconnect attempt; doubles after each attempt that never opens. */
  reconnectDelayMs?: number;
  /** Upper bound for the exponential reconnect backoff. */
  maxReconnectDelayMs?: number;
  /** Timer used to schedule reconnects (injectable for tests). */
  schedule?: (callback: () => void, delayMs: number) => void;
  createSocket?: (url: string) => WebSocketLike;
  /** Fetches a fresh auth token before each reconnect; the daemon issues a new one on restart. */
  refreshAuthToken?: () => Promise<string>;
}

export class PyError extends Error {
  backendTraceback: string | null;

  constructor(name: string, message: string, traceback: string | null) {
    super(message);
    this.name = `Backend ${name}`;
    if (traceback) {
      this.stack = `${this.stack ?? ''}\n\n${traceback}`;
    }
    this.backendTraceback = traceback;
  }
}

interface PendingCall<T> {
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  message: CallMessage;
}

const WS_OPEN = 1;

export class WSRouter {
  private runningCalls = new Map<number, PendingCall<unknown>>();
  private eventListeners = new Map<string, Set<(...args: unknown[]) => unknown>>();
  private ws?: WebSocketLike;
  private reqId = 0;
  private readonly baseUrl: string;
  private readonly reconnectDelayMs: number;
  private readonly maxReconnectDelayMs: number;
  private readonly schedule: (callback: () => void, delayMs: number) => void;
  private reconnectAttempts = 0;
  private readonly createSocket: (url: string) => WebSocketLike;
  private readonly refreshAuthToken?: () => Promise<string>;

  constructor(options: WSRouterOptions = {}) {
    this.baseUrl = options.baseUrl ?? 'ws://127.0.0.1:1337';
    this.reconnectDelayMs = options.reconnectDelayMs ?? 1000;
    this.maxReconnectDelayMs = options.maxReconnectDelayMs ?? 30_000;
    this.schedule =
      options.schedule ??
      ((callback, delayMs) => {
        setTimeout(callback, delayMs);
      });
    this.createSocket =
      options.createSocket ?? ((url: string) => new WebSocket(url) as unknown as WebSocketLike);
    this.refreshAuthToken = options.refreshAuthToken;
  }

  connect(authToken: string): Promise<void> {
    return new Promise((resolve) => {
      const ws = this.createSocket(`${this.baseUrl}/ws?auth=${encodeURIComponent(authToken)}`);
      this.ws = ws;

      ws.addEventListener('open', () => {
        this.reconnectAttempts = 0;
        resolve();
        const messages = Array.from(this.runningCalls.values()).map((pending) => pending.message);
        this.write({ type: MessageType.FullSync, messages });
      });

      ws.addEventListener('message', (rawEvent) => {
        const msgEvent = rawEvent as { data?: string };
        if (typeof msgEvent.data === 'string') {
          this.handleRawMessage(msgEvent.data);
        }
      });

      ws.addEventListener('close', () => {
        this.scheduleReconnect(authToken);
      });
    });
  }

  private scheduleReconnect(lastToken: string): void {
    const delayMs = Math.min(
      this.reconnectDelayMs * 2 ** this.reconnectAttempts,
      this.maxReconnectDelayMs,
    );
    this.reconnectAttempts += 1;
    this.schedule(() => {
      void this.reconnect(lastToken);
    }, delayMs);
  }

  private async reconnect(lastToken: string): Promise<void> {
    let token = lastToken;
    if (this.refreshAuthToken) {
      try {
        token = await this.refreshAuthToken();
      } catch (err: unknown) {
        console.warn('[ZeroDeck] failed to refresh auth token, retrying', err);
        this.scheduleReconnect(lastToken);
        return;
      }
    }
    await this.connect(token);
  }

  call<Args extends unknown[] = [], Return = void>(route: string, ...args: Args): Promise<Return> {
    const id = ++this.reqId;
    const message: CallMessage = {
      type: MessageType.Call,
      route,
      args,
      id,
    };

    return new Promise<Return>((resolve, reject) => {
      this.runningCalls.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        message,
      });
      this.write(message);
    });
  }

  callable<Args extends unknown[] = [], Return = void>(
    route: string,
  ): (...args: Args) => Promise<Return> {
    return (...args: Args) => this.call<Args, Return>(route, ...args);
  }

  addEventListener(event: string, listener: (...args: unknown[]) => unknown) {
    let set = this.eventListeners.get(event);
    if (!set) {
      set = new Set();
      this.eventListeners.set(event, set);
    }
    set.add(listener);
    return listener;
  }

  removeEventListener(event: string, listener: (...args: unknown[]) => unknown): void {
    const set = this.eventListeners.get(event);
    if (!set) return;
    set.delete(listener);
    if (set.size === 0) {
      this.eventListeners.delete(event);
    }
  }

  private handleRawMessage(raw: string): void {
    let data: MessageFromBackend;
    try {
      data = JSON.parse(raw) as MessageFromBackend;
    } catch (err: unknown) {
      console.error('[ZeroDeck] failed to parse WebSocket JSON frame', err);
      return;
    }
    switch (data.type) {
      case MessageType.Reply: {
        const pending = this.runningCalls.get(data.id);
        if (pending) {
          pending.resolve(data.result);
          this.runningCalls.delete(data.id);
        }
        this.write({ type: MessageType.ReceivedResponse, id: data.id });
        break;
      }
      case MessageType.Error: {
        const pending = this.runningCalls.get(data.id);
        if (pending) {
          pending.reject(new PyError(data.error.name, data.error.error, data.error.traceback));
          this.runningCalls.delete(data.id);
        }
        this.write({ type: MessageType.ReceivedResponse, id: data.id });
        break;
      }
      case MessageType.Discard: {
        const pending = this.runningCalls.get(data.id);
        if (pending) {
          pending.reject(new Error(`Call ${data.id} discarded by backend`));
          this.runningCalls.delete(data.id);
        }
        this.write({ type: MessageType.ReceivedResponse, id: data.id });
        break;
      }
      case MessageType.Event: {
        const listeners = this.eventListeners.get(data.event);
        if (!listeners) return;
        const event = data.event;
        const report = (err: unknown) => {
          console.error(`[ZeroDeck] error in event listener ${event}`, err);
        };
        for (const listener of listeners) {
          try {
            void Promise.resolve(listener(...data.args)).catch(report);
          } catch (err: unknown) {
            report(err);
          }
        }
        break;
      }
    }
  }

  private write(payload: MessageToBackend): void {
    if (this.ws?.readyState === WS_OPEN) {
      this.ws.send(JSON.stringify(payload));
    }
  }
}
