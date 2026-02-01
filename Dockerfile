FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
COPY rust-toolchain.toml .

RUN apt-get update && apt-get install -y --no-install-recommends \
    nodejs npm \
    && rm -rf /var/lib/apt/lists/*
RUN rustup show && rustup component add rust-src

RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN npm ci && npx svelte-kit sync && npm run build
RUN cargo build -p app --release    

FROM debian:trixie-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && apt-get clean

COPY --from=builder /app/build ./build
COPY --from=builder /app/target/release/app ./app

EXPOSE 8080
CMD ["./app"]