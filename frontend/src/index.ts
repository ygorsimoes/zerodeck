import { WSRouter } from "./wsrouter.ts";

declare global {
	interface Window {
		zerodeckAuthToken?: string;
		DeckyBackend?: WSRouter;
		deckyAuthToken?: string;
	}
}

async function bootstrap(): Promise<void> {
	const token = await fetch("http://127.0.0.1:1337/auth/token").then((r) =>
		r.text(),
	);
	window.zerodeckAuthToken = token;
	window.deckyAuthToken = token;

	const router = new WSRouter();
	window.DeckyBackend = router;
	await router.connect(token);

	console.info("[ZeroDeck:Boot] WebSocket router connected");
}

void bootstrap();
