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
		/** Captures from the route declared in `http.path`, e.g. `{id}`. */
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

	/** Reading what has arrived at one of your inboxes. */
	interface Inbox {
		/**
		 * Messages waiting at `name`, oldest first. Each is the parsed JSON a
		 * sender posted, or the raw string if it was not JSON.
		 *
		 * Throws if the inbox has expired, been drained, or never existed —
		 * those are one case on purpose, so a name reveals nothing.
		 *
		 * Scoped to whoever deployed the function, not to what is asked for:
		 * naming another account's inbox finds nothing.
		 */
		get<T = unknown>(name: string): Promise<T[]>;
	}

	/** One durable state entry. */
	interface StateEntry<T = unknown> {
		key: string;
		value: T;
		/** Increments on every successful write; the CAS token. */
		version: number;
	}

	/**
	 * Durable JSON state scoped to (owner, function name) — it survives
	 * revisions and even delete/redeploy. Single-key compare-and-set is the
	 * transaction boundary; there are no multi-key transactions.
	 */
	interface State {
		get<T = unknown>(key: string): Promise<StateEntry<T> | null>;
		/**
		 * Writes `value` only if the entry's version is exactly
		 * `expectedVersion` (`null` means "create; fail if it exists").
		 * Keys are 1–512 UTF-8 bytes; values serialize to at most 64 KiB.
		 */
		compareAndSet<T>(
			key: string,
			expectedVersion: number | null,
			value: T,
		): Promise<
			| { ok: true; version: number }
			| { ok: false; currentVersion: number | null }
		>;
		delete(
			key: string,
			expectedVersion: number,
		): Promise<{ ok: true } | { ok: false; currentVersion: number | null }>;
		/** Lexicographic by key, at most 100 entries per call. */
		list<T = unknown>(options?: {
			prefix?: string;
			cursor?: string;
			limit?: number;
		}): Promise<{ items: StateEntry<T>[]; cursor?: string }>;
	}

	/**
	 * One declared object-storage binding, exposed at
	 * `context.objects.<NAME>`. Keys live inside a namespace private to this
	 * owner and function — other functions' objects are unreachable by
	 * construction. S3 traffic here is a host capability: it does not spend
	 * the invocation's outbound-fetch allowance.
	 */
	interface ObjectStore {
		/**
		 * A presigned upload URL for exactly `contentLength` bytes with
		 * exactly this SHA-256 (64 lowercase hex chars). Create-only: PUT to
		 * an existing key fails. Send every header in `headers` verbatim.
		 * Expires in `expiresInSeconds` (15–300, default 120).
		 */
		presignPut(key: string, options: {
			contentLength: number;
			sha256: string;
			expiresInSeconds?: number;
		}): Promise<{ url: string; headers: Record<string, string>; expiresAt: number }>;
		presignGet(key: string, options?: {
			expiresInSeconds?: number;
		}): Promise<{ url: string; headers: Record<string, string>; expiresAt: number }>;
		head(key: string): Promise<{
			contentLength: number;
			sha256?: string;
			etag?: string;
			lastModified?: number;
		} | null>;
		delete(key: string): Promise<boolean>;
		/** At most 1,000 keys per call, namespace already stripped. */
		list(options?: {
			prefix?: string;
			cursor?: string;
			limit?: number;
		}): Promise<{ keys: string[]; cursor?: string }>;
	}

	/** One binding under `config.objects` — read at deploy time. */
	interface ObjectBindingConfig {
		/** Exact origin, e.g. "https://<account>.r2.cloudflarestorage.com". */
		endpoint: string;
		/** SigV4 region. Defaults to "auto" (what R2 expects). */
		region?: string;
		bucket: string;
		/** Hard per-object ceiling, enforced before a PUT is signed. */
		maxObjectBytes: number;
		/**
		 * Secret-vault entries holding the credentials. Resolved by the host;
		 * never readable from JavaScript, and refused if also listed in
		 * `config.secrets`.
		 */
		accessKeyIdSecret: string;
		secretAccessKeySecret: string;
	}

	/** Helpers for building a response. */
	interface Context {
		/** `application/json`. */
		json(body: unknown, init?: ResponseInit): Response;
		/** `text/plain; charset=utf-8`. */
		text(body: string, init?: ResponseInit): Response;
		/**
		 * Present on a deployed function. Absent when running somewhere the
		 * host lends no services — `rusted run` locally, for instance — so a
		 * handler that needs it fails saying so rather than finding nothing.
		 */
		inbox?: Inbox;
		/**
		 * The secrets this module requested via `export const config`,
		 * decrypted. Present only when the module declares `config.secrets`
		 * and runs on a server with a secret store; each declared name is
		 * guaranteed to be set, or the invocation is refused before the
		 * handler runs.
		 */
		env?: Record<string, string>;
		/**
		 * `length` bytes from the operating system's cryptographic random
		 * source — for OAuth state, PKCE verifiers, nonces, and keys, where
		 * `Math.random()` must never be used. Always present, local runs
		 * included. Length is 1 to 1024.
		 */
		randomBytes(length: number): Uint8Array;
		/**
		 * `length` random bytes as unpadded base64url — URL- and cookie-safe.
		 * 32 bytes (256 bits) encodes to 43 characters.
		 */
		randomBase64Url(length: number): string;
		/**
		 * Durable state, present only when the module declares
		 * `config.state = true` and the host supplies it.
		 */
		state?: State;
		/**
		 * Object-storage bindings declared in `config.objects`, by name.
		 * Present only for declared bindings the host supplies.
		 */
		objects?: Record<string, ObjectStore>;
	}

	/**
	 * `export const config = { … }` — read at deploy time, valid on either
	 * surface.
	 */
	interface Config {
		/**
		 * Names of secrets to decrypt into `context.env`, e.g.
		 * `["GITHUB_CLIENT_SECRET"]`. Set the values in the console under
		 * Secrets. 1–64 chars of A-Z, 0-9, '_', not starting with a digit;
		 * at most 32 names.
		 */
		secrets?: string[];
		/** Durable state. Must be exactly `true` when present. */
		state?: true;
		/** Object-storage bindings by name (`[A-Z][A-Z0-9_]{0,63}`). */
		objects?: Record<string, ObjectBindingConfig>;
	}

	/** `export const http = { … }` — read at deploy time. */
	interface Http {
		/** Defaults to the filename. */
		name?: string;
		/** Allowed methods. Defaults to ["POST"]. */
		methods?: string[];
		/** Route nested under /f/<name>, e.g. "/users/{id}". */
		path?: string;
		/**
		 * Callable without an API key even when the server requires auth.
		 * What an OAuth callback or webhook target needs — the third party
		 * calling it cannot present your key. Default: whatever the server's
		 * auth mode demands.
		 */
		public?: boolean;
	}

	/** `export const mcp = { … }` — the mcp surface, read at deploy time. */
	interface Mcp {
		/** Defaults to the filename. */
		name?: string;
		/** Serve without a key. Default: a rusted API key of the owner is required. */
		public?: boolean;
		tools: Record<string, Tool>;
	}

	/** One tool: the metadata clients list, and the handler that runs. */
	interface Tool {
		description: string;
		/** JSON Schema for the arguments. Must be an object schema. */
		inputSchema: Record<string, unknown>;
		/**
		 * Returning a string sends it as text; returning anything else sends
		 * it as JSON, with `undefined` becoming `null`. Throwing reports the
		 * error to the calling model as a tool failure.
		 */
		handler: (
			args: Record<string, unknown>,
			context: ToolContext,
		) => unknown | Promise<unknown>;
	}

	/** What a tool handler is lent. */
	interface ToolContext {
		/**
		 * Present on a deployed function. Absent when running somewhere the
		 * host lends no services — `rusted run` locally, for instance — so a
		 * handler that needs it fails saying so rather than finding nothing.
		 */
		inbox?: Inbox;
		/**
		 * The secrets this module requested via `export const config`,
		 * decrypted. Present only when the module declares `config.secrets`
		 * and runs on a server with a secret store; each declared name is
		 * guaranteed to be set, or the call is refused before the tool runs.
		 */
		env?: Record<string, string>;
		/**
		 * `length` bytes from the operating system's cryptographic random
		 * source — for OAuth state, PKCE verifiers, nonces, and keys, where
		 * `Math.random()` must never be used. Always present, local runs
		 * included. Length is 1 to 1024.
		 */
		randomBytes(length: number): Uint8Array;
		/**
		 * `length` random bytes as unpadded base64url — URL- and cookie-safe.
		 * 32 bytes (256 bits) encodes to 43 characters.
		 */
		randomBase64Url(length: number): string;
		/**
		 * Durable state, present only when the module declares
		 * `config.state = true` and the host supplies it.
		 */
		state?: State;
		/**
		 * Object-storage bindings declared in `config.objects`, by name.
		 * Present only for declared bindings the host supplies.
		 */
		objects?: Record<string, ObjectStore>;
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
