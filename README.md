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

### Setup

Create a `.env` file with the following variables:

```bash
API_KEY=<api-key>
BASE_URL=<base-url>
```

### Commands

```bash
cargo run                                        # test RLM
mprocs                                           # run server and frontend

prek run --all-files                             # run hooks
act push --bind                                  # test CI
docker build -t rlm-rs .                         # test build
docker run --rm -p 8080:8080 rlm-rs              # test run

fly secrets set API_KEY=<api-key>
fly secrets set BASE_URL=<base-url>
fly ips allocate-v4                              # allocate a dedicated IPv4 for WebRTC
fly secrets set ICE_PUBLIC_IPS=<dedicated-ipv4>
fly secrets set TURN_USERNAME=<username> TURN_CREDENTIAL=<credential>
fly redis create
fly secrets set UPSTASH_REDIS_REST_URL=<url> UPSTASH_REDIS_REST_TOKEN=<token>
fly deploy
```

## Roadmap

- [x] port rlm-minimal to Rust + RustPython
- [ ] replace OpenAI LLM endpoint with Modal
- [ ] run REPL in gVisor
- [ ] add support for depth > 1
- [ ] add
      [shared program state](https://elliecheng.com/blog/2026/01/20/enabling-rlm-with-shared-program-state/)
- [ ] add cost tracking

## Credit

- [rlm-minimal](https://github.com/alexzhang13/rlm-minimal),
  [blog post](https://alexzhang13.github.io/blog/2025/rlm/),
  [paper](https://arxiv.org/pdf/2512.24601v1)
- [gVisor](https://github.com/google/gvisor)
