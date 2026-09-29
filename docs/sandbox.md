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
| Windows | AppContainer without the three network capability SIDs, sparing localhost loopback | AppContainer ephemeral SID ACLs on allowed project, workdir, and temp roots | network: `WSAEACCES` (10013); filesystem: `ERROR_ACCESS_DENIED` (5) "Access is denied" |
| Other | not yet enforced | not yet enforced | n/a |

An AppContainer denies both axes by default, so on Windows the half of the policy that is
not requested is granted back explicitly: `--confine-fs` alone attaches the internet-client,
internet-client-server and private-network capabilities, and `--no-network` alone grants
the working directory and the temp directory. There is no "writes unconfined" setting for
an AppContainer, so a `--no-network` command on Windows can write its project and temp
files but not, say, the user's home directory. That is the one place the Windows backend
is stricter than the flag asks.

The sandbox is applied by the hidden `mermaid __sandbox-exec` launcher just before it runs
the real command, and is inherited by everything the command spawns. It fails closed: if
requested confinement cannot be applied, the command exits 126 instead of running
unconfined. On platforms without a backend the exec tool does not request confinement at
all — it logs a once-per-process warning, and `mermaid self-test` reports the real
per-platform availability.

## Read-only mode

On macOS, and on Linux kernels with Landlock ABI 6 (6.12 and later), `read_only` mode is
enforced by the kernel rather than predicted by a parser. Every shell command runs inside a
fixed read-only sandbox, applied by `mermaid __sandbox-exec --read-only`. On Linux:

- no filesystem writes anywhere, except the discard devices (`/dev/null`, `/dev/zero`,
  `/dev/full`) and the command's terminal (Landlock);
- no file metadata changes (mode, owner, extended attributes, timestamps, `chattr` flags),
  no sockets of any family (so no network and no local daemons such as Docker, D-Bus or a
  database socket), no System V or POSIX IPC, no kernel keyring, and no `io_uring`
  (seccomp, `EPERM`; internet sockets still die with `SIGSYS`);
- no signals to processes outside the sandbox (Landlock scoping), and no capabilities, even
  when Mermaid runs as root.

On macOS the launcher runs the command under a fixed, deny-default Seatbelt profile. A
write-only deny would not be read-only there, because Mach IPC lets a command change state
through a daemon without writing a file itself (`launchctl`, `defaults`, Apple Events to
another app). So everything is refused unless the profile lists it:

- allowed: running programs, every file read, sysctl reads, process information and signals
  inside the sandbox, writes to `/dev/null`, `/dev/zero` and the command's terminal, and two
  Mach services: user and group lookups, and the preferences daemon for reads (it refuses
  writes from a sandbox without `user-preference-write`);
- refused: every other write, including metadata and extended attributes, all networking
  including unix sockets, every other Mach service, Apple Events, IPC and IOKit.

Because the kernel holds that line, the policy lets any command run in `read_only` mode, not
just the ones the shell allowlists recognise as reads: `cargo metadata`, a project script, an
unfamiliar binary. A command that tries to change something fails with a permission error,
and the model is told the read-only sandbox refused it. The destructive-pattern hard-deny and
your own `deny` overrides still apply first. Every restriction is a hard requirement: there
is no best-effort degrade, and a launcher that cannot apply all of it exits 126.

Elsewhere (Windows, and Linux kernels before 6.12) `read_only` mode keeps deciding with the
shell allowlists, exactly as before. `mermaid self-test` reports which one this machine uses.
The rollout is deliberately one platform at a time: the Windows AppContainer backend needs
the same audit before it replaces its allowlists.
