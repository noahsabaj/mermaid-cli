# Mermaid Desktop

Native Mermaid command center built with Tauri 2, SvelteKit, and Tailwind CSS 4.

The desktop app is an attached client for `mermaidd`. It does not open the runtime SQLite database directly. On first launch it creates a local desktop token under Mermaid's data directory, bootstraps that token with the daemon over the Unix socket, then sends all reads and mutations through daemon commands.

Primary views:

- Needs Attention: approvals, blocked tasks, ready processes, and recent checkpoints
- Tasks: daemon task submission and task detail
- Approvals: mandatory full-detail review before approve or deny
- Processes: process actions and log tails
- Checkpoints: restore preview and restore action
- Models, memory, plugins, settings, and diagnostics

Run locally:

```sh
npm install
npm run tauri dev
```

If the app cannot attach to the daemon, install and start the user service:

```sh
mermaid daemon install --start
```

For frontend-only checks:

```sh
npm run check
npm run build
```
