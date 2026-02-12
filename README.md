# rlm-rs

RLMs in Rust using RustPython and gVisor

![icon](./assets/icon.png)

## Development

### System requirements

- [Linux](https://en.wikipedia.org/wiki/Linux) running [x86-64](https://en.wikipedia.org/wiki/X86-64) or [ARM64](https://en.wikipedia.org/wiki/AArch64) architectures. See instructions for running on AWS EC2 below.

### Installation

- [rustup](https://rustup.rs/)
- [prek](https://prek.j178.dev/installation/)

```bash
prek install
```

Linux:

```bash
sudo apt-get update && sudo apt-get install -y runsc
sudo runsc install
sudo systemctl restart docker
```

EC2:

- [aws cli](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) and [auth setup](https://docs.aws.amazon.com/cli/latest/userguide/cli-chap-authentication.html)

```bash
IAM_USER=<iam-user> make aws-setup                          # optionally specify IAM_USER to create access key, then create key pair
ARCH=arm64 INSTANCE_TYPE=t4g.medium ROOT_GB=50 make create  # optionally specify ARCH, INSTANCE_TYPE, ROOT_GB, then create instance
make conn

# in the instance
make ec2-setup
```

### Setup

Create a `.env` file with the following variables:

```bash
OPENAI_API_KEY=<api-key>
```

### Commands

Run `make help` for the full list of commands.

For both Linux and EC2 instances:

```bash
cargo test
cargo run
make app
make goose HOST=<host>
```

## Roadmap

- [x] port rlm-minimal to Rust and RustPython
- [x] unblock event loop
- [x] add support for depth > 1
- [x] add [shared program state](https://elliecheng.com/blog/2026/01/20/enabling-rlm-with-shared-program-state/)
- [ ] add per-session REPL sandboxing with gVisor

## Details

This diagram shows the async runtime flow:

![async](./assets/async.png)

When the model emits REPL code, the Tokio loop dispatches `Execute` and `GetVariable` commands through `ReplHandle` using the `mpsc` and `oneshot` channels to a dedicated REPL worker thread, which prevents RustPython `interpreter.enter` work from blocking Tokio worker threads.

This approach is better than 1) running RustPython directly on Tokio workers, which reduces concurrency under load, and 2) spawning a fresh thread per REPL call, which adds scheduling overhead and complicates state reuse. Inside Python, `llm_query(...)` returns to async model calls through the captured runtime handle, while startup context generation is offloaded with `spawn_blocking`.

## Credit

- [rlm-minimal](https://github.com/alexzhang13/rlm-minimal)
- [rlm blog post](https://alexzhang13.github.io/blog/2025/rlm/)
- [rlm paper](https://arxiv.org/pdf/2512.24601v1)
- [RustPython](https://github.com/RustPython/RustPython)
- [gVisor](https://github.com/google/gvisor)
- [goose](https://github.com/tag1consulting/goose)
