# Changelog

Every release of fleet. Each section is written when a pull request with a
`feat`, `fix` or `perf` commit is merged; see
[Releases](README.md#releases).

## [0.2.0](https://github.com/patrickkunzke/fleet/releases/tag/v0.2.0) — 2026-10-02

### Features

- Update fleet to its newest release with `fleet update`

## [0.1.0](https://github.com/patrickkunzke/fleet/releases/tag/v0.1.0) — 2026-10-01

The first release: fleet as a herdr plugin anyone can install.

### Features

- A chief that plans and delegates, started without Claude Code's editing tools
- One agent per repository, each in a herdr tab of its own, briefed on its task, its repository, what it waits on and how to report
- A shared board of epics, tasks, dependencies and background processes, one per fleet, read and written through `fleet board`
- The `/board` skill, loaded into every agent fleet starts
- Messages between agents, recorded on the board and delivered to the recipient's prompt
- A live graph of the crew, with the board beside it; `↵` or a click goes to an agent's tab
- Each agent's task, and the board at a glance, in herdr's sidebar, and a notification when a task is blocked, in review or done
- Runs: after a restart, fleet brings the crew back, each agent into its own conversation
- Workspaces that open in fleet mode by themselves, from `~/.claude-fleet/auto-open`
- The doctor action, which checks herdr, Claude Code and the settings fleet needs
