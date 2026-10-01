# KOS-exec

Repositories: [kos-exec](https://github.com/KATECH-JYHAN/kos-exec) · [kos-comm](https://github.com/KATECH-JYHAN/kos-comm) · [kos-safety](https://github.com/KATECH-JYHAN/kos-safety)

An execution manager for automotive Linux that **runs and supervises applications isolated by ASIL domain**, plus a library for writing those applications.
For every configuration key, launcher behavior, and API, see [docs/reference.md](docs/reference.md).

> v0.1.0, under development · Linux only (cgroup v2)

## Usage

### 1. Build

Clone [KOS-comm](https://github.com/KATECH-JYHAN/kos-comm) and [KOS-safety](https://github.com/KATECH-JYHAN/kos-safety) next to KOS-exec.

```bash
git clone https://github.com/KATECH-JYHAN/kos-exec.git
git clone https://github.com/KATECH-JYHAN/kos-comm.git comm
git clone https://github.com/KATECH-JYHAN/kos-safety.git safety
cd kos-exec
```

```bash
cargo build --release                          # target/release/kos, libkos_exec.{so,a}
cargo install --path crates/kos-exec-cli       # installs `kos` into ~/.cargo/bin
```

### 2. Write a configuration

Describe the domains (dedicated cores, RT priority, resource limits) and the apps that run in them in TOML.

```toml
[[domain]]
id = "adas"
asil = "D"
cores = [0, 1]
rt_priority = 50

[[domain]]
id = "ivi"
asil = "QM"
cores = [2, 3]
max_pids = 64

[[app]]
id = "adas.camera"
binary = "/opt/kos/app/adas/camera/camera"
domain = "adas"

[[app]]
id = "adas.fusion"
binary = "fusion"
domain = "adas"
depends_on = ["adas.camera"]     # starts after camera

[app.restart]
max_retries = 3                  # restarts after a crash
watchdog_ms = 500                # treated as hung and restarted after 500ms without a heartbeat

[[app]]
id = "ivi.media"
binary = "media"
domain = "ivi"
```

Examples: `crates/kos-exec/tests/scenarios/*.toml`

### 3. Run and manage

```bash
kos launch demo.toml --supervise &     # start and stay resident (--supervise: restart on crash or hang)
kos status                             # state, PID, and restart count of each app
kos stop adas.fusion                   # stop / start, restart
kos suspend ivi.media                  # pause / resume
kos incidents                          # crash and hang records
kos shutdown                           # stop everything
```

In production, run `kos launch` as a systemd service ([example](docs/reference.md#running-as-a-service)). Core pinning, RT priority, and IRQ isolation are applied only when the launcher has the required privileges; otherwise they are skipped with a warning.

### 4. Write an app

Apps also run without the launcher. When started by the launcher, they automatically receive their app ID, parameters, and thread settings.

```rust
use kos_exec::{kos_app, runtime::run, RuntimeConfig};

kos_app! {
    name: Counter,
    data: { count: u32 = 0 },
    on_init: |self, ctx| { ctx.log_info("init"); Ok(()) },
    on_run:  |self, ctx| { self.count += 1; Ok(()) },
}

fn main() -> kos_exec::Result<()> {
    run(Counter::new(), RuntimeConfig::from_env("demo.counter"))
}
```

- Periodic/event threads, topic pub/sub, service calls: [docs/reference.md](docs/reference.md#library)
- C/C++: `crates/kos-exec-ffi/include/kos_exec.h`, example `crates/kos-exec-ffi/examples/ffi_demo.c`

## Performance

Average wake-up latency of a critical app (10ms period, 2ms of work), measured while varying the load from other apps (non-critical, NC).
Native is plain Linux with no configuration; KOS runs the same app in a KOS domain.

**Idle system**

| NC load | Native | KOS |
|--------:|-------:|----:|
| 0% | 16–66μs | **1.0–1.5μs** |
| 100% | 31–798μs | **1.1–1.4μs** |
| 200% | 0.6–35ms, 38% missed | **1.1–1.6μs** |

**With other workloads running** (a Yocto build on the same machine)

| NC load | Native | KOS |
|--------:|-------:|----:|
| 0% | 29μs–1.1ms, 1.8% missed | **1.6–37μs** |
| 100% | 4.6–179ms, 63% missed | **1.6–2.1μs** |
| 200% | 4–317ms, 98% missed | **1.5–2.6μs** |

- KOS missed no deadlines in any case; its latency stays at a few μs regardless of the load from other apps.
- KOS cannot help when the critical domain itself is overloaded: plan for at most 80% utilization per domain.

<sub>Core Ultra 7 356H, Linux 7.0. 1–16 critical apps (≤ 80% utilization per core). Reference values, not measured on dedicated hardware. Methodology and pub/sub results: [docs/reference.md](docs/reference.md#performance-details).</sub>

## Author

Jun-young Han, Senior Researcher — KATECH SDV Platform Research Center · jyhan@katech.re.kr

## License

Copyright (c) 2026 Jun-young Han. Licensed under the [Apache License 2.0](LICENSE).
