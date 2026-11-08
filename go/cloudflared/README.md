# cloudflared on HopOS — the example

A HopOS node behind NAT, with no inbound port and no kernel, made publicly
reachable by Cloudflare Tunnel. This is cloudflared's **own** `tunnel run`,
unmodified, running as a slot image on its own core — or sharing one, if you put
it in a sharegroup.

The point of this example is not the tunnel. It is that "just a Go program in a
slot" also holds for somebody else's Go program: cloudflared's entire dependency
tree — a forked quic-go, gopacket, gopsutil, gRPC, OpenTelemetry, Prometheus,
capnproto — compiles for `GOOS=tamago` untouched. Two small platform fallbacks
are all that is missing, and they are in [patch/](patch/).

```
30 MB  cloudflared-arm64-tamago.elf     (Pi 4/5, UEFI)
29 MB  cloudflared-riscv64-tamago.elf   (LicheeRV Nano — builds, but see Limits)
```

## Build it

```sh
tools/prepare-cloudflared.sh   # once, and after every version bump in go.mod
tools/build.sh                 # host tests + both slot images into out/
```

`prepare-cloudflared.sh` copies the pinned cloudflared out of the module cache
into `build/cloudflared-patched` (gitignored) and drops the two fallback files
in. **Until you run it, every go command in this module fails** with
"replacement directory ./build/cloudflared-patched does not exist" — including
`go test`. That is the trade for not maintaining a fork; see *Why a patch* below.

## Run it

The app dials **out**, so it publishes no port at all. Config comes from the job
spec env, under cloudflared's own env names — so the Cloudflare documentation
applies verbatim.

| env | what |
|---|---|
| `TUNNEL_TOKEN` | named tunnel; ingress comes from the Cloudflare dashboard. Empty ⇒ quick tunnel |
| `TUNNEL_URL` | the local service to expose. Default `http://$HOPOS_HOST` |
| `TUNNEL_TRANSPORT_PROTOCOL` | `http2` (our default) or `quic` |
| `TUNNEL_LOGLEVEL` | `info` by default |
| `TUNNEL_METRICS` | where the metrics/readiness server binds. Default `<own-ip>:20241` |
| `CFD_EXTRA_ARGS` | free-form extra flags, split on spaces |

Plus the rest of [`cfd.Bridged`](internal/cfd/cfd.go): a slot's env is *not* the
process env (applib reads it from the control page), so the app copies a
documented allowlist into the process env with `os.Setenv`. applib cannot
enumerate its env, only look names up, so an allowlist is the only option —
extending it means adding a name there.

**Quick tunnel** — no account, no token, free `trycloudflare.com` hostname. With
no `TUNNEL_URL` it exposes `http://$HOPOS_HOST`, which is port 80 of this node:
by default this puts the [welcome](../../apps/welcome/) page on the public internet.

```json
{"name":"cloudflared","driver":"hop","artifacts":[
  {"url":"https://…/cloudflared-arm64-tamago.elf","match":{"node.arch":"arm64"}},
  {"url":"https://…/cloudflared-riscv64-tamago.elf","match":{"node.arch":"riscv64"}}],
 "memory_limit":268435456}
```

**Named tunnel** — your own hostname, ingress configured in the dashboard:

```json
{"name":"cloudflared","driver":"hop","artifacts":[
  {"url":"https://…/cloudflared-arm64-tamago.elf","match":{"node.arch":"arm64"}},
  {"url":"https://…/cloudflared-riscv64-tamago.elf","match":{"node.arch":"riscv64"}}],
 "memory_limit":268435456,
 "env":{"TUNNEL_TOKEN":"eyJhIjoi…"}}
```

`hop logs cloudflared` shows everything: the app's own lines (mode, protocol, own
IP, target) *and* cloudflared's own zerolog output, a quick tunnel's public URL
included. That is not this app's doing — HopOS provides the floor. A write to fd
1/2 goes through `runtime.write1` → `goos.Printk`, and `board/hopslot` hands that
to the sink `applib` installs at `Init`: this app's task log. So nothing here
redirects anything.

The tunnel's state is also on the metrics server (`/ready`, `/metrics`,
`/quicktunnel`) for whatever wants to poll rather than read.

## Why a patch, and why it is two files

cloudflared's `diagnostic` package (used by `tunnel diag`) only has
implementations for linux, darwin and windows: a traceroute collector that shells
out via `os/exec`, and a system collector that reads `/proc`, sysctl or WMI. On
any other platform those types simply do not exist and the package fails to
compile. HopOS has no `traceroute` binary, no `os/exec` and no host to interrogate
— the Go runtime *is* the OS — so the honest implementations are the ones in
[patch/](patch/): they return "not available on this platform". That is exactly
what an upstream PR would add.

Go's `-overlay` would be the clean way to inject them, but it refuses to touch
files under `GOMODCACHE` ("Files beneath GOMODCACHE must not be replaced"), hence
the copy-and-patch script. Alternatives, if this ever gets annoying: send the two
files upstream, or keep a fork and point the replace at it — one line in go.mod.

Note the other two replaces in [go.mod](go.mod): cloudflared has its **own**
replaces for `urfave/cli` and `quic-go`, and a dependency's replaces do not apply
transitively. Without repeating them, its code does not compile — it uses API
that only exists in those forks.

## What it took to actually run

**It runs**: a named tunnel over `http2` from a QEMU HopOS node, confirmed
connected on the Cloudflare side (2026-07-31). Getting there took two fixes, and
one of them was hidden by a third problem.

**1. `tunnel.Init` — a nil `*BuildInfo`.** Registering `tunnel.Commands()` is not
enough: cloudflared's own `main.go` also sets a handful of package globals that
the tunnel path then dereferences. Without `tunnel.Init(bInfo, …)`, `StartServer`
panics immediately in `cliutil.(*BuildInfo).Log`. The app now runs the same
sequence upstream does (`tunnel`, `updater`, `management`, `token`, `tracing`,
`RegisterBuildInfo`) plus their own `QUIC_GO_DISABLE_ECN=1`.

**2. The metrics listener — a slot has no loopback.** cloudflared binds its
metrics/readiness server to `localhost:0` by default, and the slot's netstack
(the SDK's own; `go/apps-release.sh` checks that no gVisor is linked) has
exactly one NIC with the slot's own IP. Upstream's `virtual` runtime turns that
into `0.0.0.0:0`, which the stack rejects with `bind: bad local
address`. It needs a **concrete address and a concrete port**, so the app passes
`--metrics <own-ip>:20241` (see `DefaultMetricsPort`; `TUNNEL_METRICS` overrides).

**3. Why neither was visible — since fixed in HopOS itself.** `board/hopslot`'s
`printk` used to be a no-op (a caged core has no UART MMIO), so runtime panics
*and* anything on stdout/stderr were dropped and HOP reported only `exit code 2`.
It now forwards to `appboard.PrintkSink`, which `applib` points at the task log —
so a panic is simply the last log line of the task, and app output lands in the
app's own log where it belongs. What this app still does, and what it no longer
needs to:

- `recover()` logs the panic and its stack into the ring, with the **summary
  last**, because the last ring line is what HOP echoes in its exit message
  (`last="…"`). Reason on the console, stack in `hop logs`.
- `cli.ErrWriter` and `cli.OsExiter` still point at the ring. These were the
  invisible exits: urfave/cli writes the fatal error to `ErrWriter` and then goes
  straight to `os.Exit`, so `Run` never returns — which is how the metrics error
  stayed hidden. `ErrWriter` would now land in the task log by itself (its default
  is `os.Stderr`, and that reaches the sink), so it is belt and braces: one clean
  ring line instead of byte-wise printk. `OsExiter` still earns its keep — it
  names the exit code and leaves through `app.Exit`.
- **Gone**: an `os.Pipe` that redirected stdout/stderr into the ring, and a
  `CFD_HOLD_ON_PANIC` env that parked a crashed app so its stack could be fetched
  before the task disappeared. The printk sink retired the first (`os.Pipe` does
  not exist on tamago anyway — "pipe: not implemented"), and HOP keeping a
  finished task's logs for five minutes retired the second. Both were scaffolding
  around a missing floor; the floor exists now.

Remaining unknowns, in order:

1. **QUIC** is still unmeasured. We default to `http2` because it is TCP+TLS over
   the same path as any other outbound slot connection; QUIC is UDP with socket
   options and datagram sizes that have to survive the per-slot netstack and
   HOP's masquerade. quic-go already warns it cannot grow its receive buffer
   (`wanted 7168 kiB, got 0`), which is harmless on http2.
   `TUNNEL_TRANSPORT_PROTOCOL=quic` to try it.
2. **Graceful shutdown.** HOP's kill is abrupt (kill flag → `EXITED` → CPU_OFF),
   so `graceShutdownC` never closes and the connector does not deregister; the
   edge notices by itself.
3. **Trust store** worked out: there is no `/etc/ssl`, so `x509.SystemCertPool()`
   is empty and the app imports `golang.org/x/crypto/x509roots/fallback` — the
   same bundle the apploader uses. TLS to the edge is fine, which also confirms
   HOP's SNTP clock offset arrives before the first handshake.

Also verified without a node: both images build and link; the generated command
line is checked against the **pinned** cloudflared's real flag tree and every
bridged env name against its real `EnvVars`
([internal/cfd/flags_test.go](internal/cfd/flags_test.go)), so an upstream rename
breaks a test instead of a node.

## Limits

- **30 MB image** ⇒ `memory_limit` of ~256 MB. That is a Pi-class demo. The
  riscv64 build exists so the code keeps building for both, but a LicheeRV Nano
  has a 175 MB pool and one slot: it does not fit.
- Published to the `apps` release by [go/apps-release.sh](../apps-release.sh)
  (arm64 only), so a job spec can point straight at it:

  ```
  https://github.com/hop-os/HopOS/releases/download/apps/cloudflared-arm64-tamago.elf
  ```

  The script runs `tools/prepare-cloudflared.sh` itself before the tamago
  build, so a release picks up a version bump in go.mod without anyone
  remembering the step.
- `tunnel diag`, `tunnel login` and the service-install subcommands are along for
  the ride but cannot work here; the diagnostic collectors say so when asked.
