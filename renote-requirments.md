# Renote requirements for Rusted

Implement the two generic host capabilities Renote Shared Folders require. Keep Rusted product-agnostic: do **not** add Renote routes, share tables, encryption, membership, or capability-token semantics to Rusted core. Renote will implement those in a public `renote-share` TypeScript function.

Source product decision: `/Users/iluxav/Library/CloudStorage/GoogleDrive-iluxa.v@gmail.com/My Drive/ideas/renote-infinite-canvas-of-docs.md`, section **end-to-end encrypted Shared Folders**.

## 1. Declared runtime capabilities

Extend `export const config`:

```ts
export const config = {
  state: true,
  objects: {
    SHARES: {
      endpoint: "https://<account>.r2.cloudflarestorage.com",
      region: "auto",
      bucket: "renote-shares",
      maxObjectBytes: 67108864,
      accessKeyIdSecret: "R2_ACCESS_KEY_ID",
      secretAccessKeySecret: "R2_SECRET_ACCESS_KEY",
    },
  },
};
```

- `state` is optional and must be exactly `true` when present.
- Object binding names must match `[A-Z][A-Z0-9_]{0,63}`.
- Credential fields name entries in Rusted's existing owner secret vault. Resolve them host-side; never place raw credentials in `context.env`, JavaScript, stored function metadata, responses, errors, or logs.
- Reject a module that also lists either binding credential in `config.secrets`; storage credentials may be used only by the host binding.
- Reject unknown/malformed configuration during `verify` and `push`.
- Persist declarations with the function revision. Refuse invocation before JavaScript runs if a declared deployed capability cannot be supplied.
- Undeclared capabilities must be absent from `Context` and `ToolContext`.

## 2. `context.state`

Provide durable JSON state scoped by `(function owner, stable function name)`. It persists across revisions and delete/redeploy. State deletion must require a separate explicit admin/CLI purge operation.

```ts
interface StateEntry<T = unknown> {
  key: string;
  value: T;
  version: number;
}

interface State {
  get<T = unknown>(key: string): Promise<StateEntry<T> | null>;
  compareAndSet<T>(
    key: string,
    expectedVersion: number | null, // null means create only
    value: T,
  ): Promise<
    | { ok: true; version: number }
    | { ok: false; currentVersion: number | null }
  >;
  delete(
    key: string,
    expectedVersion: number,
  ): Promise<{ ok: true } | { ok: false; currentVersion: number | null }>;
  list<T = unknown>(options?: {
    prefix?: string;
    cursor?: string;
    limit?: number;
  }): Promise<{ items: StateEntry<T>[]; cursor?: string }>;
}
```

Required semantics:

- Back with a SQLx migration in the existing Postgres database. CAS and delete must be single atomic SQL operations; never implement read-then-write in application code.
- Keys are UTF-8, 1–512 bytes. Values must be JSON-compatible and at most 64 KiB serialized. `list` is lexicographically ordered and capped at 100 entries per call.
- Add `max_state_keys` and `max_state_bytes` to the existing plan limits and enforce them per function, transactionally. A refused write must change neither data nor accounting; the storage layer must not hard-code product-tier values.
- Cache reads where useful and invalidate every instance through the existing Postgres `LISTEN/NOTIFY` channel. Correctness must not depend on cache TTL.
- Single-key CAS is the transaction boundary; multi-key transactions are out of scope.

## 3. `context.objects`

Expose each declared binding at `context.objects.<BINDING>`:

```ts
interface ObjectStore {
  presignPut(key: string, options: {
    contentLength: number;
    sha256: string; // 64 lowercase hex characters
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
  list(options?: {
    prefix?: string;
    cursor?: string;
    limit?: number;
  }): Promise<{ keys: string[]; cursor?: string }>;
}
```

Required semantics:

- Use a maintained Rust S3/SigV4 library; do not implement signing manually.
- Automatically prefix every key with an opaque owner/function namespace. JavaScript must never see or escape this prefix. Reject empty keys, `..`, control characters, leading `/`, and keys over 1,024 UTF-8 bytes.
- Deployed endpoints must be exact origins on a server-admin allowlist; bindings are disabled by default. This prevents a function from using the binding as SSRF. Permit local HTTP endpoints only in local/test mode.
- Presigned URLs expire in 15–300 seconds. PUT is create-only, signs the exact content length and SHA-256 checksum, and returns every header the client must send. No multipart upload in v1.
- `maxObjectBytes` is required per binding, must be between 1 byte and 5 GiB, and is enforced before signing. Renote initially configures 64 MiB; aggregate Shared Folder quotas remain Renote protocol state.
- `head`, `delete`, and `list` operate only inside the injected namespace. Cap list results at 1,000 keys.
- S3 traffic is a host capability, not JavaScript `fetch`, and does not consume the function's outbound-request allowance. It remains bounded by the invocation deadline.
- Never log signed URLs, their query strings, storage credentials, or caller authorization headers. Redact provider errors before returning them to JavaScript.

## 4. Local development

When a module declares these capabilities under `rusted run`:

- Supply an in-memory state adapter that survives hot reload and resets when the process exits.
- Supply an isolated temporary-directory object adapter. Local `presignPut` and `presignGet` must return expiring dev-server transfer URLs with the same method, checksum, length, namespace, and create-only behavior as production.
- Do not require Postgres, S3, or production credentials for this local path.

## 5. Types, documentation, and compatibility

- Update the canonical `rusted.d.ts`, generated/scaffolded copies, runtime glue, stored function records, pull/list/detail JSON, and README.
- Add a minimal example function using state CAS plus an object PUT/HEAD/GET flow.
- Existing functions that declare neither capability must behave exactly as before. Preserve inbox, secrets, HTTP, and MCP behavior.

## 6. Required tests

Add repository/domain tests, not browser UI tests, covering:

- configuration validation and undeclared-capability absence;
- owner/function isolation and persistence across revisions;
- simultaneous CAS with exactly one winner;
- aggregate state-limit accounting and cross-instance invalidation;
- object namespace traversal rejection and cross-owner isolation;
- endpoint allowlisting and missing-secret refusal;
- PUT expiry, create-only behavior, exact length, and checksum enforcement;
- signed-URL/credential redaction;
- local-adapter parity for the Renote-required flow;
- unchanged behavior for existing functions.

Completion requires `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` to pass. Do not commit or deploy; leave that to the user.
