# Secrets per Environment

Design spec for introducing an **environments** concept to rusted, so one deployed
function can resolve different secret values depending on which environment it was
invoked through. Motivating case: `RENOTE_APP_ORIGIN` needs to be
`http://localhost:3000` when the app is developed locally against the hosted
function, and `https://renote.app` in production — without any branching inside the
function code.

## Concept

- An environment is a simple string owned by the account (e.g. `prod`, `stage`).
- Every account has `prod` by default. It is a real row, not an implicit absence,
  and it cannot be deleted.
- Users can create additional environments from the console/CLI.
- v1 scope: an environment is a **secrets overlay only** — same deployed code,
  different vault values. Per-environment code versions (deploy a candidate to
  stage first) is an explicit non-goal for v1, but the routing choice below keeps
  that door open for v2.

## Routing

The env travels in the URL, marked with `@` so it can never collide with a
function name:

```
https://rusted.sh/f/settle            → prod (unchanged, all existing URLs keep working)
https://rusted.sh/f/@stage/settle     → stage
https://rusted.sh/f/@stage/settle/x   → stage, sub-path "/x" passed as rest
```

Why not a bare segment (`/f/stage/settle`): the router already serves both
`/f/:name` and `/f/:name/*rest` (`crates/rusted-server/src/api.rs`), and functions
receive the sub-path. A bare segment is ambiguous — `/f/stage/settle` already means
"function `stage`, path `/settle`" today, and a function literally named `stage`
would shadow the environment. The `@` marker resolves this with zero DB lookups
(function names must never be allowed to start with `@` — enforce in name
validation).

Invoking `@<env>` for an environment that does not exist → 404 before any function
resolution.

## Function context

Expose the resolved environment to the handler:

```js
export default async function handler(request, context) {
  context.currentEnv; // "prod" | "stage" | ... — ALWAYS a string, never null
}
```

- Do **not** use `null` to mean prod. Two representations for one value forces
  every consumer to normalize; `context.currentEnv === "prod"` is the check.
- Naming: `context.env` is already taken by secrets. Keep the two visually
  distinct (`currentEnv` is acceptable; `envName` or a `context.meta` field are
  alternatives) — do not overload `env`.

## Secrets resolution

Secrets are stored **under an environment**. Function code stays unconditional:

```js
export const config = { secrets: ["RENOTE_APP_ORIGIN"] };
// handler just reads context.env.RENOTE_APP_ORIGIN
```

Resolution: the invocation's environment selects the vault namespace; the declared
name resolves to that environment's value.

### Missing values: refuse, never empty

A declared secret that is **unset in the resolved environment** must keep the
existing all-or-nothing guarantee: the invocation is refused *before* the handler
runs, callers get the generic `missing_secrets` error, and the exact missing names
appear in `rusted logs`.

Explicitly rejected alternatives:

- **Resolve to empty string** — silently reintroduces the half-configured-vault
  bug class the refusal exists to prevent.
- **Silent fallback to prod values** — worst case: stage code quietly running with
  prod credentials, which is exactly the incident this feature is meant to
  prevent.

If inheritance is ever wanted, it must be an explicit per-secret opt-in flag
("inherit from prod"), never a default.

### Host-only secrets follow the same env

Object-binding credentials (`accessKeyIdSecret` / `secretAccessKeySecret`),
`context.seal` / `context.open` keys (`keySecret`), and MCP OAuth introspection
credentials all name vault entries. They must resolve through the invocation's
environment too — otherwise a stage function signs and decrypts with prod keys.

## Storage & cache

- Secrets table gains an `env` column; uniqueness becomes `(user, env, name)`.
- Migration: all existing secret rows get `env = 'prod'`.
- The per-user decrypted cache becomes per `(user, env)`; the LISTEN/NOTIFY
  invalidation payload gains the env dimension so a re-save in one environment
  invalidates only that environment's cache.
- Encryption is unchanged: values sealed with AES-256-GCM under
  `RUSTED_SECRETS_KEY` before touching Postgres.

## Local dev (`rusted run`)

Unchanged: no vault locally, `context.env` is `undefined`, and `context.currentEnv`
should report a fixed value (e.g. `"local"`) so code can distinguish it. The
existing fallback pattern still applies:

```js
const origin = context.env?.RENOTE_APP_ORIGIN ?? "http://localhost:3000";
```

## Renote use case, end state

- Prod vault (`prod` env): `RENOTE_APP_ORIGIN = https://renote.app`
- Stage vault (`stage` env): `RENOTE_APP_ORIGIN = http://localhost:3000`
- Localhost app calls `https://rusted.sh/f/@stage/renote-auth`; production calls
  `https://rusted.sh/f/renote-auth`. Zero conditions in function code.
