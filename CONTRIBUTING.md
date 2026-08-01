# Contributing

## Getting set up

```bash
make db          # postgres:18 via docker compose, port 5457
make check       # fmt, clippy, and the full test suite
make i           # build and install the `rusted` binary
```

`make check` needs the database running; it creates a disposable database per
test and drops them afterwards. A C compiler is required (QuickJS and blake3
compile C), and Rust 1.85 or newer.

## Sign your commits

Contributions are accepted under the [Developer Certificate of
Origin](https://developercertificate.org/). Sign off each commit:

```bash
git commit -s -m "your message"
```

That adds a `Signed-off-by:` line certifying you wrote the patch, or otherwise
have the right to contribute it under this project's licenses. Pull requests
without a sign-off can't be merged.

Note which license applies to what you're touching — `rusted-engine` and
`rusted-cli` are Apache-2.0, `rusted-server` is AGPL-3.0. See
[LICENSE.md](LICENSE.md).

## What makes a change easy to accept

- **Tests first.** Backend behavior gets a test that fails before the fix and
  passes after. Console pages are exempt from UI tests, but a route that
  renders should stay in the smoke test.
- **`make check` passes.** Formatting, clippy with warnings denied, and every
  test.
- **Explain the why, not the what, in comments.** The code says what it does.
- **Keep limits honest.** If a change affects what a plan allows, say so in the
  pull request — those numbers are a promise to users.
