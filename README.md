# rlm-rs

RLMs in Rust and RustPython + gVisor

![icon](./icon.png)

## Development

### Installation

- [rustup](https://rustup.rs/)
- [prek](https://prek.j178.dev/installation/)

```bash
prek install
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
prek run --all-files                             # run hooks
```

## Roadmap

- [x] port rlm-minimal to Rust and RustPython
- [ ] run REPL in gVisor
- [ ] unblock event loop
- [ ] add support for depth > 1
- [ ] add
      [shared program state](https://elliecheng.com/blog/2026/01/20/enabling-rlm-with-shared-program-state/)
- [ ] replace OpenAI LLM endpoint with Modal
- [ ] add cost tracking

## Credit

- [rlm-minimal](https://github.com/alexzhang13/rlm-minimal)
- [rlm blog post](https://alexzhang13.github.io/blog/2025/rlm/)
- [rlm paper](https://arxiv.org/pdf/2512.24601v1)
- [RustPython](https://github.com/RustPython/RustPython)
- [gVisor](https://github.com/google/gvisor)
