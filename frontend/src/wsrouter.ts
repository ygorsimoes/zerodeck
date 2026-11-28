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

/**
 * Capability advertised by `/auth/token` when `FULL_SYNC` replays use at-most-once semantics for
 * potentially mutating calls.
 */
export const FULL_SYNC_AT_MOST_ONCE = 'full-sync-at-most-once';

/** Auth token plus the daemon capabilities announced alongside it. */
export interface AuthInfo {
  token: string;
  capabilities: string[];
}

/** Parses the `/auth/token` body and its capability header into an {@link AuthInfo}. */
export function parseAuthResponse(token: string, capabilitiesHeader: string | null): AuthInfo {
  const capabilities =
    capabilitiesHeader === null
      ? []
      : capabilitiesHeader
          .split(',')
          .map((capability) => capability.trim())
          .filter((capability) => capability.length > 0);
  return { token, capabilities };
}

/**
 * Rejection for a call that was sent before a disconnect and whose outcome the daemon cannot
 * confirm. Callers must refresh state instead of assuming the call did or did not happen.
 */
export class IndeterminateOutcomeError extends Error {
  readonly code = 'IndeterminateCallOutcome';

  constructor(message: string) {
    super(message);
    this.name = 'IndeterminateOutcomeError';
  }
}

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
  refreshAuthToken?: () => Promise<string | AuthInfo>;
  /**
   * Rejects a call that has not been answered within this many milliseconds. Unset by default,
   * because plugin methods may legitimately run for a long time.
   */
  callTimeoutMs?: number;
  /**
   * Identifies this page load to the daemon so its `FULL_SYNC` reply cache never mixes call ids
   * from another session. Defaults to a fresh `crypto.randomUUID()`.
   */
  sessionId?: string;
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
  /** Whether a `CALL` frame was written to an open socket at least once. */
  sent: boolean;
  timer?: ReturnType<typeof setTimeout>;
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
  private lastAuth: string | AuthInfo = '';
  private readonly createSocket: (url: string) => WebSocketLike;
  private readonly refreshAuthToken?: () => Promise<string | AuthInfo>;
  private readonly callTimeoutMs?: number;
  readonly sessionId: string;

  constructor(options: WSRouterOptions = {}) {
    this.sessionId = options.sessionId ?? crypto.randomUUID();
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
    this.callTimeoutMs = options.callTimeoutMs;
  }

  /**
   * Fetches a token and connects, retrying with backoff while the daemon is unreachable (e.g. when
   * Steam's UI loads before `zerodeckd` is listening). Requires `refreshAuthToken`.
   */
  start(): void {
    if (!this.refreshAuthToken) {
      throw new Error('WSRouter.start() requires the refreshAuthToken option');
    }
    void this.reconnect('');
  }

  connect(authToken: string, replayCapable = false): Promise<void> {
    this.lastAuth = replayCapable
      ? { token: authToken, capabilities: [FULL_SYNC_AT_MOST_ONCE] }
      : authToken;
    return new Promise((resolve) => {
      const ws = this.createSocket(
        `${this.baseUrl}/ws?auth=${encodeURIComponent(authToken)}&session=${encodeURIComponent(this.sessionId)}`,
      );
      this.ws = ws;

      ws.addEventListener('open', () => {
        this.reconnectAttempts = 0;

        const sent = Array.from(this.runningCalls.values()).filter((pending) => pending.sent);
        if (!replayCapable) {
          // The daemon cannot distinguish a replay from a new request, so resending a call that
          // may have been applied would break at-most-once semantics.
          for (const pending of sent) {
            this.settle(pending.message.id);
            pending.reject(
              new IndeterminateOutcomeError(
                `Call ${pending.message.route} (#${pending.message.id}) was sent before the ` +
                  'connection dropped and the daemon cannot confirm its outcome; it may already ' +
                  'have been applied. Refresh state before retrying.',
              ),
            );
          }
        }

        this.write({
          type: MessageType.FullSync,
          messages: replayCapable ? sent.map((pending) => pending.message) : [],
        });

        // Calls created while the socket was down were never sent, so they are ordinary requests.
        for (const pending of this.runningCalls.values()) {
          if (!pending.sent) {
            pending.sent = this.write(pending.message);
          }
        }

        resolve();
      });

      ws.addEventListener('message', (rawEvent) => {
        const msgEvent = rawEvent as { data?: string };
        if (typeof msgEvent.data === 'string') {
          this.handleRawMessage(msgEvent.data);
        }
      });

      ws.addEventListener('close', () => {
        this.scheduleReconnect();
      });
    });
  }

  private scheduleReconnect(): void {
    const delayMs = Math.min(
      this.reconnectDelayMs * 2 ** this.reconnectAttempts,
      this.maxReconnectDelayMs,
    );
    this.reconnectAttempts += 1;
    this.schedule(() => {
      void this.reconnect(this.lastAuth);
    }, delayMs);
  }

  private async reconnect(lastAuth: string | AuthInfo): Promise<void> {
    let auth = lastAuth;
    if (this.refreshAuthToken) {
      try {
        auth = await this.refreshAuthToken();
      } catch (err: unknown) {
        console.warn('[ZeroDeck] failed to refresh auth token, retrying', err);
        this.scheduleReconnect();
        return;
      }
    }
    const info = typeof auth === 'string' ? parseAuthResponse(auth, null) : auth;
    await this.connect(info.token, info.capabilities.includes(FULL_SYNC_AT_MOST_ONCE));
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
      const pending: PendingCall<unknown> = {
        resolve: resolve as (value: unknown) => void,
        reject,
        message,
        sent: false,
      };
      if (this.callTimeoutMs !== undefined) {
        const timeoutMs = this.callTimeoutMs;
        pending.timer = setTimeout(() => {
          if (this.runningCalls.delete(id)) {
            reject(new Error(`Call ${route} (#${id}) timed out after ${timeoutMs}ms`));
          }
        }, timeoutMs);
      }
      this.runningCalls.set(id, pending);
      pending.sent = this.write(message);
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

  /** Removes a pending call and cancels its timeout, returning it if it was still pending. */
  private settle(id: number): PendingCall<unknown> | undefined {
    const pending = this.runningCalls.get(id);
    if (pending) {
      clearTimeout(pending.timer);
      this.runningCalls.delete(id);
    }
    return pending;
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
        this.settle(data.id)?.resolve(data.result);
        this.write({ type: MessageType.ReceivedResponse, id: data.id });
        break;
      }
      case MessageType.Error: {
        const error =
          data.error.name === 'IndeterminateCallOutcome'
            ? new IndeterminateOutcomeError(data.error.error)
            : new PyError(data.error.name, data.error.error, data.error.traceback);
        this.settle(data.id)?.reject(error);
        this.write({ type: MessageType.ReceivedResponse, id: data.id });
        break;
      }
      case MessageType.Discard: {
        this.settle(data.id)?.reject(new Error(`Call ${data.id} discarded by backend`));
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

  /** Writes a frame when the socket is open and returns whether it was handed to the socket. */
  private write(payload: MessageToBackend): boolean {
    if (this.ws?.readyState !== WS_OPEN) {
      return false;
    }
    try {
      this.ws.send(JSON.stringify(payload));
      return true;
    } catch {
      return false;
    }
  }
}
