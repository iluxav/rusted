// Durable state + object storage, together: a tiny content-addressed file
// index. CAS guards the index entry; the bytes themselves go to the object
// store via presigned URLs, so they never pass through the function.
//
// Locally (`rusted run index.js`) state is in-memory and objects live in a
// temp directory — same semantics, no setup. Deployed, state is Postgres and
// objects are the bucket below (set the two credential secrets in the
// console, and ask the server admin to allowlist the endpoint).

export const config = {
  state: true,
  objects: {
    FILES: {
      endpoint: "https://<account>.r2.cloudflarestorage.com",
      region: "auto",
      bucket: "my-files",
      maxObjectBytes: 67108864, // 64 MiB
      accessKeyIdSecret: "R2_ACCESS_KEY_ID",
      secretAccessKeySecret: "R2_SECRET_ACCESS_KEY",
    },
  },
};

// One route per verb instead of branching on request.method by hand.
export const app = rusted
  .app({ name: "file-index" })
  .get("/", list)
  .post("/", reserve);

// What the index knows, plus what the store actually holds.
async function list(request, context) {
  const files = context.objects.FILES;
  const entry = await context.state.get("index");
  const stored = await files.list({ limit: 100 });
  return context.json({ index: entry?.value ?? [], stored: stored.keys });
}

// POST { name, size, sha256 } → an upload slot plus an index entry.
async function reserve(request, context) {
  const files = context.objects.FILES;
  const { name, size, sha256 } = await request.json();

  // Reserve the name in the index with compare-and-set: two concurrent
  // uploads of the same name race, exactly one wins.
  const current = await context.state.get("index");
  const names = current?.value ?? [];
  if (names.includes(name)) {
    return context.json({ error: "name_taken" }, { status: 409 });
  }
  const wrote = await context.state.compareAndSet(
    "index",
    current?.version ?? null,
    [...names, name],
  );
  if (!wrote.ok) {
    return context.json({ error: "index_moved_retry" }, { status: 409 });
  }

  // The upload slot: create-only, exact size, exact checksum, short-lived.
  const put = await files.presignPut(`blobs/${sha256}`, {
    contentLength: size,
    sha256,
    expiresInSeconds: 120,
  });
  // Already stored? (Content-addressed keys make re-uploads no-ops.)
  const existing = await files.head(`blobs/${sha256}`);
  const get = await files.presignGet(`blobs/${sha256}`);

  return context.json({
    upload: existing ? null : put, // PUT the bytes with exactly put.headers
    download: get.url,
    indexVersion: wrote.version,
  });
}
