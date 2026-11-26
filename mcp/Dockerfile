# Orvyn's trading MCP servers (Rust). Build from the repository root:
#   docker build -f mcp/Dockerfile -t orvyn-mcp .
#   docker run -e BSC_RPC_URL=https://... -p 3400:3400 orvyn-mcp
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p orvyn-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/orvyn-mcp /usr/local/bin/orvyn-mcp
RUN useradd --create-home orvyn
USER orvyn
ENV PORT=3400
EXPOSE 3400
CMD ["orvyn-mcp"]
