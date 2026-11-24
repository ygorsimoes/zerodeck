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
type MessageFromBackend =
	| ReplyMessage
	| ErrorMessage
	| DiscardMessage
	| EventMessage;

export interface WebSocketLike {
	readonly readyState: number;
	addEventListener(type: string, listener: (event: unknown) => void): void;
	send(data: string): void;
}

export interface WSRouterOptions {
	baseUrl?: string;
	reconnectDelayMs?: number;
	createSocket?: (url: string) => WebSocketLike;
}

export class PyError extends Error {
	backendTraceback: string | null;

	constructor(name: string, message: string, traceback: string | null) {
		super(message);
		this.name = `Backend ${name}`;
		if (traceback) {
			this.stack = `${this.stack ?? ""}\n\n${traceback}`;
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
	private eventListeners = new Map<
		string,
		Set<(...args: unknown[]) => unknown>
	>();
	private ws?: WebSocketLike;
	private reqId = 0;
	private readonly baseUrl: string;
	private readonly reconnectDelayMs: number;
	private readonly createSocket: (url: string) => WebSocketLike;

	constructor(options: WSRouterOptions = {}) {
		this.baseUrl = options.baseUrl ?? "ws://127.0.0.1:1337";
		this.reconnectDelayMs = options.reconnectDelayMs ?? 5000;
		this.createSocket =
			options.createSocket ??
			((url: string) => new WebSocket(url) as unknown as WebSocketLike);
	}

	connect(authToken: string): Promise<void> {
		return new Promise((resolve) => {
			const ws = this.createSocket(
				`${this.baseUrl}/ws?auth=${encodeURIComponent(authToken)}`,
			);
			this.ws = ws;

			ws.addEventListener("open", () => {
				resolve();
				const messages = Array.from(this.runningCalls.values()).map(
					(pending) => pending.message,
				);
				this.write({ type: MessageType.FullSync, messages });
			});

			ws.addEventListener("message", (rawEvent) => {
				const msgEvent = rawEvent as { data?: string };
				if (typeof msgEvent.data === "string") {
					this.handleRawMessage(msgEvent.data);
				}
			});

			ws.addEventListener("close", () => {
				setTimeout(() => {
					void this.connect(authToken);
				}, this.reconnectDelayMs);
			});
		});
	}

	call<Args extends unknown[] = [], Return = void>(
		route: string,
		...args: Args
	): Promise<Return> {
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

	removeEventListener(
		event: string,
		listener: (...args: unknown[]) => unknown,
	): void {
		const set = this.eventListeners.get(event);
		if (!set) return;
		set.delete(listener);
		if (set.size === 0) {
			this.eventListeners.delete(event);
		}
	}

	private handleRawMessage(raw: string): void {
		const data = JSON.parse(raw) as MessageFromBackend;
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
					pending.reject(
						new PyError(
							data.error.name,
							data.error.error,
							data.error.traceback,
						),
					);
					this.runningCalls.delete(data.id);
				}
				this.write({ type: MessageType.ReceivedResponse, id: data.id });
				break;
			}
			case MessageType.Discard: {
				this.runningCalls.delete(data.id);
				this.write({ type: MessageType.ReceivedResponse, id: data.id });
				break;
			}
			case MessageType.Event: {
				const listeners = this.eventListeners.get(data.event);
				if (!listeners) return;
				for (const listener of listeners) {
					void Promise.resolve(listener(...data.args)).catch((err: unknown) => {
						console.error(
							`[ZeroDeck] error in event listener ${data.event}`,
							err,
						);
					});
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
