# OS Sandbox

Model-run shell commands can be confined by the operating system, independently of the
approval policy. Two dimensions, each with its own flag (or config key):

- `--no-network` (`safety.network = "deny"`): omits and blocks all web tools on every
  platform. It prevents model-run commands from reaching the network: Linux seccomp-BPF
  and macOS Seatbelt kill/deny internet sockets while sparing local `AF_UNIX` IPC; Windows
  AppContainers omit network capabilities while sparing localhost (`127.0.0.1`) loopback.
- `--confine-fs` (`safety.filesystem = "project"`): write-class filesystem access is allowed
  only beneath the project root, the working directory, the system temp directory, and (on
  unix) `/dev`. Reads and execution stay unrestricted.
- `--sandbox`: both at once.

Enforcement is per-platform, behind one facade:

| Platform | Shell network deny | Write confinement | Denial signature |
| --- | --- | --- | --- |
| Linux | seccomp-BPF kill-switch: creating an `AF_INET`/`AF_INET6` socket dies with `SIGSYS` | Landlock (kernel 5.13+; best-effort no-op with a warning on older kernels) | network: precise (`SIGSYS`); filesystem: hedged permission-error text |
| macOS | Seatbelt (`sandbox-exec`) allow-default profile with `(deny network*)`, sparing `AF_UNIX` | Seatbelt `deny file-write*` outside the allowed roots, matched on both the literal and canonicalized path (so `TMPDIR` firmlinks work) | both hedged: `EPERM` "Operation not permitted", no signal |
| Windows | AppContainer capability omission, sparing localhost loopback | AppContainer ephemeral SID ACLs on allowed project, workdir, and temp roots | network: `WSAEACCES` (10013); filesystem: `ERROR_ACCESS_DENIED` (5) "Access is denied" |
| Other | not yet enforced | not yet enforced | n/a |

The sandbox is applied by the hidden `mermaid __sandbox-exec` launcher just before it runs
the real command, and is inherited by everything the command spawns. It fails closed: if
requested confinement cannot be applied, the command exits 126 instead of running
unconfined. On platforms without a backend the exec tool does not request confinement at
all — it logs a once-per-process warning, and `mermaid self-test` reports the real
per-platform availability.
