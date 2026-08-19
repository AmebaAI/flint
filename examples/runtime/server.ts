declare const Bun: {
	file(path: string): { text(): Promise<string> };
	serve(options: {
		hostname: string;
		port: number;
		fetch(request: Request): Response | Promise<Response>;
	}): void;
	write(path: string, data: string): Promise<number>;
};

const counterPath = "/workspace/invocation-count";
const json = (body: unknown, status = 200) => Response.json(body, { status });
let counterQueue = Promise.resolve();

const incrementCount = () => {
	const operation = counterQueue.then(async () => {
		try {
			let count = 0;
			try {
				const contents = await Bun.file(counterPath).text();
				const stored = Number.parseInt(contents.trim(), 10);
				if (Number.isSafeInteger(stored) && stored >= 0) {
					count = stored;
				}
			} catch (error) {
				const code =
					error && typeof error === "object" && "code" in error
						? error.code
						: undefined;
				if (code !== "ENOENT") {
					return Promise.reject(error);
				}
			}
			const next = count + 1;
			await Bun.write(counterPath, `${next}\n`);
			return next;
		} catch (error) {
			return Promise.reject(error);
		}
	});
	counterQueue = operation.then(
		() => undefined,
		() => undefined,
	);
	return operation;
};

Bun.serve({
	hostname: "0.0.0.0",
	port: 8080,
	async fetch(request) {
		try {
			const { pathname } = new URL(request.url);

			if (request.method === "GET" && pathname === "/ping") {
				return json({ status: "Healthy" });
			}

			if (request.method === "POST" && pathname === "/invocations") {
				await request.arrayBuffer();
				const count = await incrementCount();
				return json({ message: "hello from flint-runtime-example", count });
			}

			return json({ message: "not found" }, 404);
		} catch {
			return json({ message: "request failed" }, 500);
		}
	},
});
