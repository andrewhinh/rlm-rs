# rlm-rs

RLMs in Rust and gVisor

## Development

### Installation

- [rustup](https://rustup.rs/)
- [npm](https://nodejs.org/en/download/)
- [prek](https://prek.j178.dev/installation/)
- [act](https://nektosact.com/installation/index.html)
- [fly](https://fly.io/docs/flyctl/install/)

Create an [Open Relay TURN server](https://www.metered.ca/tools/openrelay/)
account [here](https://dashboard.metered.ca/login?tool=turnserver) for TURN
server credentials.

```bash
npm i
prek install
fly auth login
```

### Commands

```bash
cargo run
mprocs                                           # run server and frontend

prek run --all-files                             # run hooks
act push --bind                                  # test CI
docker build -t rlm-rs .                         # test build
docker run --rm -p 8080:8080 rlm-rs              # test run

fly ips allocate-v4                              # allocate a dedicated IPv4 for WebRTC
fly secrets set ICE_PUBLIC_IPS=<dedicated-ipv4>
fly secrets set TURN_USERNAME=<username> TURN_CREDENTIAL=<credential>
fly redis create
fly secrets set UPSTASH_REDIS_REST_URL=<url> UPSTASH_REDIS_REST_TOKEN=<token>
fly deploy
```

## Credit

- [rlm-minimal](https://github.com/alexzhang13/rlm-minimal)
- [rlm blog post](https://alexzhang13.github.io/blog/2025/rlm/)
- [rlm paper](https://arxiv.org/pdf/2512.24601v1)
- [verifiers rlm example](https://github.com/PrimeIntellect-ai/verifiers/blob/main/verifiers/envs/experimental/rlm_env.py)
