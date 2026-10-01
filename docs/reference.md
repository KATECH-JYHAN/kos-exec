# KOS-exec Reference

For a quick start, see the [README](../README.md).

## Running as a service

Run `kos launch` as a systemd service in production. `Delegate=yes` gives KOS its own cgroup, which it needs to guarantee the behavior described under [Launcher behavior](#launcher-behavior).

```ini
# /etc/systemd/system/kos.service
[Unit]
Description=KOS execution manager

[Service]
ExecStart=/usr/local/bin/kos launch /etc/kos/launch.toml --supervise
Delegate=yes
KillMode=control-group
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now kos
sudo kos status          # the control socket (/run/kos/control.sock) is root-only
```

When started from a shell, `kos launch` re-executes itself in a delegated systemd scope automatically.

## Isolation

Isolation applied according to the domain configuration (skipped with a warning when privileges are missing):

| Target | Applied | Required privilege |
|--------|---------|--------------------|
| All apps | Pinned to the domain's `cores` | None |
| Domains with `rt_priority` | SCHED_FIFO | `CAP_SYS_NICE` |
| ASIL < C without `rt_priority` | nice 19 | None |
| Cores of ASIL ≥ C domains | IRQs moved away, C-states off (restored when the launcher exits) | root |
| `memory_limit_mb`/`max_pids`/`cpu_quota` | cgroup v2 limits | Delegation (re-executed in a systemd scope automatically) |

Each app runs in its own cgroup, `kos.slice/apps/<domain>/<app>`, and domain limits are set on `kos.slice/apps/<domain>`.

## Launch TOML

**`[[domain]]`**

| Key | Required | Description |
|-----|:--------:|-------------|
| `id`, `asil`, `cores` | Yes | `asil`: `QM`/`A`/`B`/`C`/`D` |
| `rt_priority` | | SCHED_FIFO 1–99 |
| `memory_limit_mb`, `max_pids`, `cpu_quota` | | cgroup limits (`cpu_quota`: 1–100% of one CPU) |

**`[[app]]`**

| Key | Required | Description |
|-----|:--------:|-------------|
| `id`, `binary`, `domain` | Yes | An undefined `domain` is an error |
| `args`, `depends_on` | | Dependencies start first; cycles are an error |
| `[app.restart]` | | `max_retries` (default 3), `backoff_ms` (default 100, doubled each time), `backoff_max_ms` (default 5000), `watchdog_ms` (hang detection, off by default) |
| `[app.params]` | | Passed to the app as `KOS_PARAM_<key>` (`ctx.param()`) |
| `[[app.thread]]` | | `name`, `period_ms`/`event_topic`, `priority`, `cpu_affinity`, `subs`, `pubs` — overrides the app's thread with the same name |
| `[app.schedule]` | | When the app runs (below) |

**`[app.schedule]`** (`type`)

| type | At launcher start | After a clean exit (exit 0) | Extra keys |
|------|:-----------------:|-----------------------------|------------|
| (none) / `boot` | Runs | Not run again | |
| `periodic` | Runs | Runs again next period | `period_ms` |
| `event` | Waits | Runs again on the next trigger | `trigger` = `signal:<name>` · `state:<PARKED\|DRIVING\|CHARGING\|EMERGENCY>` · `topic:<topic>` |
| `cron` | Waits | Runs again at the next matching minute | `expr` = `"M H * * *"` (minute and hour only, local time) |

- `signal:` fires on `kos signal <name>`, `state:` on `kos state <STATE>` (when the value changes), and `topic:` when new data is published. Triggers that arrive while the app is running are ignored.
- An app stopped with `kos stop` is not run by its schedule until `kos start`.

`binary` lookup: absolute path → `$KOS_APP_DIR/<domain>/<binary>/<binary>` (default `/opt/kos/app`) → `PATH`. If `<binary>.sha256` exists next to an absolute path, the hash is verified.

## Launcher behavior

| Situation | Behavior |
|-----------|----------|
| App crash | With `--supervise`: restarted after a backoff up to `max_retries`, then ERROR. Without it: ERROR immediately. Recorded as an incident in both cases |
| App hang (`watchdog_ms`) | SIGKILL when the heartbeat stops, then handled like a crash |
| `kos suspend` / `resume` | SIGTSTP → runtime apps call `on_suspend` and stop (SIGSTOP if not stopped within 1s) / SIGCONT → `on_resume` |
| stop · restart · suspend | Applied to every process of the app: its cgroup (including children that call `setsid`) and its process group |
| Launcher exit (`kos shutdown`, SIGINT/SIGTERM) | Apps stopped in reverse dependency order, SHM left by apps removed, IRQ/C-state settings restored |
| Abnormal launcher exit (including `kill -9`) | A separate reaper process kills all app cgroups and process groups |

> **The behavior above is guaranteed only when apps are run with `kos launch`** (from a shell or as a [systemd service](#running-as-a-service)). If you embed `Launcher` in your own program, run that program with its own delegated cgroup (systemd `Delegate=yes`); otherwise KOS leaves cgroups untouched and children that call `setsid`/`setpgid` can escape stop and cleanup.

- **Heartbeat**: `runtime::run` apps beat every cycle; `ThreadManager`/`Node` apps beat only while every thread is making progress (a single stuck thread is detected).
  Apps that never send a heartbeat (plain binaries, apps wrapped in `sh -c`) are not watched.
- Environment variables passed to apps: `KOS_APP_ID`, `KOS_DOMAIN`, `KOS_PARAM_<key>`
- Paths (first match wins)

| | Path |
|--|------|
| Control socket (0600) | `KOS_CONTROL_SOCKET` → `$KOS_DATA_DIR/control.sock` → `/run/kos/control.sock` (root) → `$XDG_RUNTIME_DIR/kos/control.sock` |
| Incident log | `KOS_INCIDENT_LOG` → `$KOS_DATA_DIR/incidents.log` → `/var/log/kos/incidents.log` (root) → `~/.local/state/kos/incidents.log` (rotated to `.1` every 1MB) |
| Service sockets | `KOS_SERVICE_DIR` → `$KOS_DATA_DIR/svc` → `/run/kos/svc` (root) → `$XDG_RUNTIME_DIR/kos/svc` |

## CLI

```
kos launch <TOML> [--supervise] [--domain <D>] [--app <ID>]
kos status [APP_ID] | start | stop | restart | suspend | resume <APP_ID> | shutdown
kos signal <NAME> | state <STATE> | incidents [-n N]
kos shm status [--toml FILE] [--topic NAME]... | info <TOPIC> | cleanup [--dry-run]
```

- `kos launch` stays resident and opens the control socket; the other commands send requests through it. `--app`/`--domain` start only that app or domain plus its dependencies.
- `kos shm` inspects KOS-comm topics in `/dev/shm` without a launcher. SHM names are hashes, so only topics known to the launcher, `--toml`, or `--topic` show their names; the rest show `?`.

```
$ kos shm status
TOPIC           MSG    QUEUE  PUBLISHED     PUBLISHER  READERS  OVERFLOW    MISS
adas/camera/frame  4100B   8/8      81      1886072        0        73       0
? (/kos_q_32fa3a80)  256B  8/8      10  1030139(dead)       1         2       0
```

## Library

**App** — `on_init`, then `on_run` repeatedly. On `Err`, `on_error` decides (`Restart`/`Terminate`/`Ignore`). `on_suspend`/`on_resume` on suspend and resume. Example in the [README](../README.md#4-write-an-app).

**Threads** — periodic execution on absolute time (missed periods are skipped). Subscribed topics are read before each run; written topics are published after it.

```rust
let mut node = Node::new("fusion", NodeConfig::periodic(Duration::from_millis(10)));
node.set_transport(Arc::new(ShmTransport::new("fusion")));

node.create_thread(
    ThreadConfig::periodic("control", Duration::from_millis(5))
        .with_priority(Priority::Critical)
        .with_subs(vec!["adas/obj"]).with_pubs(vec!["adas/cmd"]),
    ThreadCallbacks::new(|ctx| {
        if ctx.is_fresh("adas/obj") {
            let obj: Obstacle = ctx.read("adas/obj");
            ctx.write("adas/cmd", &plan(obj));
        }
    }),
)?;
node.create_thread(
    ThreadConfig::event("on_cmd", "adas/cmd"),
    ThreadCallbacks::new(|ctx| { let _cmd: Cmd = ctx.read("adas/cmd"); }),
)?;
node.spin()?;
```

- `Priority`: `Critical` → FIFO 80, `High` → FIFO 60, `Normal` → inherited, `Low` → nice 10
- `MainTask` restarts failed threads and shuts the node down when the same thread fails `max_thread_failures` times (default 3).
- Messages are `#[repr(C)]` POD types up to 4096 bytes. A subscriber connects even if it starts before the publisher; with KOS-comm it also reconnects when the publisher restarts.
- `ctx.read_ago::<T>(topic, n)`: the value published n times before the latest (KOS-comm keeps the last 8)

**Transport backends** — the `Transport` trait; switch with `set_transport()`.

| | Purpose |
|--|---------|
| `ShmTransport` | KOS-comm SHM (default) |
| `Iox2Transport` | iceoryx2 (`--features iceoryx2`) |
| `CheckedTransport` | Enforces ASIL write rules on a wrapped backend (a lower-ASIL app cannot write a higher-ASIL topic) |
| `MockTransport` | Tests |

Cross-process example: `cargo run --example shm_relay -- pub|sub <topic>` (switch with `KOS_TRANSPORT=iceoryx2`)

**Services (request/reply)** — cross-process over Unix sockets. Concurrent clients each get their own reply.

```rust
let _srv = ServiceServer::advertise("adas/plan", |req| Ok(plan(req)))?;
let reply = kos_exec::service::call("adas/plan", &req, Duration::from_millis(100))?;
```

**Logging** — `ctx.log_info/log_warn/log_error` → stderr (`[INFO] <app_id>: msg`)

**C FFI** — `crates/kos-exec-ffi/include/kos_exec.h`, example `crates/kos-exec-ffi/examples/ffi_demo.c`

| | Functions |
|--|-----------|
| Callback app | `kos_app_run`, `kos_param_*`, `kos_log_*` |
| pub/sub | `kos_advertise`, `kos_publish`, `kos_subscribe`, `kos_recv` |
| Node/threads | `kos_node_new`, `kos_create_thread`, `kos_node_spin`, `kos_ctx_read`/`write`/`read_ago` |
| Services | `kos_advertise_service`, `kos_call_service` (`KOS_ERR_TIMEOUT`/`NOT_FOUND`/`TRUNCATED`/`REMOTE`) |

## Performance details

```bash
sudo -E cargo bench -p kos-exec --bench deadline_stress     # migration_cost, pinning_vs_cpuset, sleep_vs_nosleep, transport_compare
```

For the Native vs KOS tables, see the [README](../README.md#performance). 1–16 critical apps (≤ 80% utilization per core), 2 seconds per measurement.

- KOS keeps latency at a few μs regardless of other apps' load. With Native, latency grows to milliseconds and deadlines are missed as load increases.
- Isolation cannot help when the critical domain itself is full: at 100% per core the average is 0.6–0.8ms, and at 120% 96% of deadlines are missed → **plan for at most 80% domain utilization**.
- This is why domains list their `cores` explicitly: with only a cpuset range, the kernel packed tasks onto one core and latency reached 113μs (3.5μs with explicit pinning).

**pub/sub backends** (`transport_compare`) — 1 publisher + N subscriber processes, 1kHz, SCHED_FIFO 80, subscribers wait for new-data notifications:

| 64B–4KB | KOS-comm | iceoryx2 0.10 |
|---------|---------:|--------------:|
| p50 latency (1 / 16 subscribers) | 16–27μs / 35–70μs | 27–53μs / 34–108μs |
| p99 latency | 23–174μs | 57–212μs |
| Subscriber CPU | 0.7–2.8% | 1.6–5.0% |

- KOS-comm wakes all subscribers with a single futex call; iceoryx2 notifies through a separate event service. With 16 subscribers the two are similar.

## Known issues

- `ctx.read_ago` is not supported by the iceoryx2 backend
