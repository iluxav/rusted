CARGO ?= cargo
BIN   := target/release/rusted

.PHONY: all build test lint fmt fmt-check check serve install uninstall i clean help db db-clean css editor-js release

all: check build

build: ## Build the release `rusted` binary
	$(CARGO) build --release -p rusted-cli

db: ## Start the local Postgres (docker compose)
	docker compose up -d --wait db

db-clean: ## Drop accumulated rusted_test_* databases
	docker exec rusted-postgres psql -U rusted -d postgres -tAc \
	  "select datname from pg_database where datname like 'rusted_test_%'" \
	  | xargs -I% docker exec rusted-postgres psql -U rusted -d postgres -c 'drop database "%"'

test: db db-clean ## Run the full test suite (needs the database)
	$(CARGO) test --release
	@$(MAKE) --no-print-directory db-clean

lint: ## Clippy on all targets; warnings are errors
	$(CARGO) clippy --release --all-targets -- -D warnings

fmt: ## Format the workspace
	$(CARGO) fmt --all

fmt-check: ## Fail if formatting is off
	$(CARGO) fmt --all -- --check

check: fmt-check lint test ## Everything a CI gate would run

serve: build ## Run the server (functions on :7411, admin API on :7412)
	$(BIN) serve

# --locked: rolldown's crates don't follow semver, so a fresh resolution mixes
# oxc versions that don't compile together. The workspace lockfile is the one
# combination known to build.
install: ## Install `rusted` into ~/.cargo/bin
	$(CARGO) install --locked --path crates/rusted-cli

uninstall: ## Remove `rusted` from ~/.cargo/bin
	$(CARGO) uninstall rusted-cli

i: build install ## Shorthand: build and install

# Patch releases only — vX.Y.Z becomes vX.Y.Z+1 from the last tag reachable on
# main. Major and minor bumps are a human decision: edit the crate versions,
# commit "vX.Y.0", tag, and push by hand. The tag push is what triggers the
# release workflow, so a mistake here means deleting a published release —
# hence the branch/sync/cleanliness guards.
release: ## Cut a patch release: bump crate versions, commit, tag, push
	@set -eu; \
	git diff --quiet && git diff --cached --quiet \
	  || { echo "error: working tree not clean"; exit 1; }; \
	branch=$$(git rev-parse --abbrev-ref HEAD); \
	[ "$$branch" = main ] || { echo "error: release from main (on $$branch)"; exit 1; }; \
	git fetch -q origin main; \
	[ "$$(git rev-parse HEAD)" = "$$(git rev-parse origin/main)" ] \
	  || { echo "error: main is not in sync with origin/main"; exit 1; }; \
	last=$$(git describe --tags --abbrev=0 --match 'v*'); \
	old=$${last#v}; \
	new=$$(echo "$$old" | awk -F. '{printf "%d.%d.%d", $$1, $$2, $$3+1}'); \
	echo "releasing v$$old -> v$$new"; \
	perl -pi -e "s/^version = \"\Q$$old\E\"/version = \"$$new\"/" \
	  crates/rusted-cli/Cargo.toml crates/rusted-engine/Cargo.toml crates/rusted-server/Cargo.toml; \
	$(CARGO) update -q -p rusted-cli -p rusted-engine -p rusted-server; \
	git add Cargo.lock crates/rusted-cli/Cargo.toml crates/rusted-engine/Cargo.toml crates/rusted-server/Cargo.toml; \
	git commit -q -m "v$$new"; \
	git tag "v$$new"; \
	git push origin main "v$$new"; \
	echo "pushed v$$new — waiting for the release workflow"; \
	sleep 10; \
	run_id=$$(gh run list --workflow=release.yml --branch "v$$new" --limit 1 --json databaseId -q '.[0].databaseId'); \
	[ -n "$$run_id" ] || { echo "error: no workflow run found for v$$new — check GitHub Actions"; exit 1; }; \
	gh run watch "$$run_id" --exit-status; \
	echo "v$$new published — deploy with .do/deploy.sh"

editor-js: ## Rebuild the vendored editor assets (Monaco + workers + esbuild-wasm)
	cd crates/rusted-server/editor && npm install --no-fund --no-audit
	cd crates/rusted-server/editor && npx esbuild entry.js --bundle --minify --format=iife --outfile=../assets/editor.js --loader:.ttf=file '--asset-names=[name]'
	cd crates/rusted-server/editor && npx esbuild node_modules/monaco-editor/esm/vs/editor/editor.worker.js --bundle --minify --outfile=../assets/editor.worker.js
	cd crates/rusted-server/editor && npx esbuild node_modules/monaco-editor/esm/vs/language/typescript/ts.worker.js --bundle --minify --outfile=../assets/ts.worker.js
	cd crates/rusted-server/editor && cp "$$(find node_modules/monaco-editor -name codicon.ttf | head -1)" ../assets/codicon.ttf
	cd crates/rusted-server/editor && cp node_modules/esbuild-wasm/esbuild.wasm ../assets/esbuild.wasm && cp node_modules/esbuild-wasm/lib/browser.min.js ../assets/esbuild.min.js

css: ## Recompile the Tailwind sheet inlined into every page (run after template edits)
	cd crates/rusted-server/tailwind && npx -y tailwindcss@3.4.17 -c tailwind.config.js -i input.css -o ../templates/app.css --minify

clean:
	$(CARGO) clean

help: ## List available targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'
