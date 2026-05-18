FROM rust:slim-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y libsqlite3-dev pkg-config && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build -p yatch-server --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libsqlite3-0 ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /data
COPY --from=builder /build/target/release/yatch /usr/local/bin/yatch
ENV DB_PATH=/data/yatch.db
EXPOSE 5000
ENTRYPOINT ["yatch"]
