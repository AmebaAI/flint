declare const Bun: {
	serve(options: {
		hostname: string;
		port: number;
		fetch(request: Request): Response | Promise<Response>;
	}): void;
};

const json = (body: unknown, status = 200) => Response.json(body, { status });

Bun.serve({
	hostname: "0.0.0.0",
	port: 8080,
	async fetch(request) {
		const { pathname } = new URL(request.url);

		if (request.method === "GET" && pathname === "/ping") {
			return json({ status: "Healthy" });
		}

		if (request.method === "POST" && pathname === "/invocations") {
			await request.arrayBuffer();
			return json({ message: "hello from flint-runtime-example" });
		}

		return json({ message: "not found" }, 404);
	},
});
