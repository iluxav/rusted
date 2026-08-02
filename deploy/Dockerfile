# Runs a published release rather than building from source: compiling rolldown
# with LTO wants more memory than a small VPS has, and CI has already done it.
FROM debian:bookworm-slim

ARG VERSION=latest
ARG TARGETARCH

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "no rusted build for ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    if [ "${VERSION}" = "latest" ]; then \
      url="https://github.com/iluxav/rusted/releases/latest/download/rusted-${target}.tar.gz"; \
    else \
      url="https://github.com/iluxav/rusted/releases/download/${VERSION}/rusted-${target}.tar.gz"; \
    fi; \
    curl -fsSL "$url" -o /tmp/rusted.tar.gz; \
    tar xzf /tmp/rusted.tar.gz -C /tmp; \
    install -m 755 "/tmp/rusted-${target}/rusted" /usr/local/bin/rusted; \
    rm -rf /tmp/rusted*

# Nothing here needs root.
RUN useradd --system --create-home --uid 10001 rusted
USER rusted

ENTRYPOINT ["rusted"]
CMD ["serve"]
