# Licensing

rusted is open source under two licenses, split along the line between what
runs on your machine and what runs the service.

| Part | License | Full text |
|---|---|---|
| `crates/rusted-engine` — the JavaScript runtime | Apache-2.0 | [LICENSE-APACHE](LICENSE-APACHE) |
| `crates/rusted-cli` — the `rusted` command | Apache-2.0 | [LICENSE-APACHE](LICENSE-APACHE) |
| `crates/rusted-server` — server, console, billing | AGPL-3.0-only | [LICENSE-AGPL](LICENSE-AGPL) |

Everything else in the repository — documentation, scripts, workflows, and
example functions — is Apache-2.0.

## What this means

**Building on the CLI or the engine**: Apache-2.0. Embed it, ship it inside a
closed product, fork it — no obligation beyond the notice. These are the parts
you install and run locally, and we want them everywhere.

**Running the server**: AGPL-3.0. Self-host it for yourself, your team, or your
company freely. If you modify it and offer it to others over a network, the
AGPL asks you to publish those modifications. That keeps improvements flowing
back without stopping anyone from running their own.

If the AGPL doesn't work for your situation, ask — a commercial license is
available.

## Contributing

Contributions are accepted under the [Developer Certificate of
Origin](https://developercertificate.org/): sign off your commits with
`git commit -s`, which certifies you wrote the patch or otherwise have the
right to submit it under these licenses. See [CONTRIBUTING.md](CONTRIBUTING.md).
