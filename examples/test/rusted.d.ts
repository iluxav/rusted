// Type declarations for rusted handlers.
//
// Written by `rusted types`. This describes the runtime exactly as it is, which
// is smaller than a browser or Node: `fetch` and `console` are reduced, and
// nothing else global exists. Prefer these types over lib.dom, which would
// promise methods this engine does not have.
//
// tsconfig:
//   { "compilerOptions": { "lib": ["ES2020"], "types": [], "strict": true } }
//
// Including "DOM" would redeclare fetch and console with fuller shapes than the
// engine implements, so code would typecheck and then fail at runtime.

declare namespace Rusted {
	/** The incoming request, already parsed by the host. */
	interface Request {
		/** Uppercase, e.g. "POST". */
		method: string;
		/** Lowercased header names. */
		headers: Record<string, string>;
		/** Parsed query string. Repeated keys keep the last value. */
		query: Record<string, string>;
		/** Captures from the route declared in `config.path`, e.g. `{id}`. */
		params: Record<string, string>;
		/** The raw body. Always a string, empty when there is no body. */
		body: string;
		/** `JSON.parse(body)`. Rejects on invalid JSON. */
		json<T = unknown>(): Promise<T>;
	}

	/** Status and headers for a response. */
	interface ResponseInit {
		/** 200–599. Anything outside that range is refused by the host. */
		status?: number;
		/**
		 * Extra headers. Framing headers — content-length, transfer-encoding,
		 * connection, keep-alive, upgrade, te, trailer, host — are refused, as
		 * are values containing line breaks.
		 */
		headers?: Record<string, string>;
	}

	/** Opaque; build one with `context.json` or `context.text`. */
	interface Response {
		readonly __rustedResponse: true;
	}

	/** Helpers for building a response. */
	interface Context {
		/** `application/json`. */
		json(body: unknown, init?: ResponseInit): Response;
		/** `text/plain; charset=utf-8`. */
		text(body: string, init?: ResponseInit): Response;
	}

	/** `export const http = { … }` — read at deploy time. */
	interface Http {
		/** Defaults to the filename. */
		name?: string;
		/** Allowed methods. Defaults to ["POST"]. */
		methods?: string[];
		/** Route nested under /f/<name>, e.g. "/users/{id}". */
		path?: string;
	}

	/**
	 * Returning a `Response` sets the content type and status. Returning a
	 * string sends it as-is; returning anything else sends it as JSON, with
	 * `undefined` becoming `null`.
	 */
	type Handler = (
		request: Request,
		context: Context,
	) => Response | string | unknown | Promise<Response | string | unknown>;

	/** What this runtime's `fetch` resolves to — not the standard Response. */
	interface FetchResponse {
		ok: boolean;
		status: number;
		headers: Record<string, string>;
		text(): Promise<string>;
		json<T = unknown>(): Promise<T>;
	}

	interface FetchInit {
		method?: string;
		headers?: Record<string, string>;
		body?: string;
	}
}

/**
 * Outbound HTTP. http and https only; addresses that resolve to private ranges
 * are refused. Each plan caps how many calls one invocation may make.
 *
 * This is a reduced fetch: no `body` stream, `blob`, `arrayBuffer`,
 * `formData`, `url`, `statusText`, or redirect handling.
 */
declare function fetch(
	url: string,
	init?: Rusted.FetchInit,
): Promise<Rusted.FetchResponse>;

/**
 * Captured per invocation and returned with it, not written to stdout. Capped
 * at 100 entries of 1024 characters; beyond that, calls are dropped.
 *
 * Only these four exist — no `debug`, `table`, `trace`, `time`, or `group`.
 */
declare const console: {
	log(...args: unknown[]): void;
	info(...args: unknown[]): void;
	warn(...args: unknown[]): void;
	error(...args: unknown[]): void;
};
