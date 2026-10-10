# cpm-planner — MCP server (stdio). Multi-stage build → slim runtime.
FROM rust:1.99.0-slim-trixie AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked --bin cpm-planner && cp target/release/cpm-planner /cpm-planner

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 app \
    && mkdir -p /data && chown app:app /data
COPY --from=build /cpm-planner /usr/local/bin/cpm-planner
USER app
ENV CPM_PLANNER_DB=/data/cpm-planner.db
VOLUME ["/data"]
LABEL io.modelcontextprotocol.server.name="io.github.praxec/cpm-planner"
# The gateway spawns this container with `docker run -i` and speaks MCP over stdio.
ENTRYPOINT ["cpm-planner"]
