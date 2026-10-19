---
description: Start a fleet agent as a desktop session, in the sidebar under this chief
argument-hint: <name> [repo] [task]
---

I am asking you, the fleet's chief, to start the agent `$ARGUMENTS` now, as a
session of the Claude desktop app. This message is my go-ahead for that
start.

1. Find what the agent is for. `$ARGUMENTS` is its name, and maybe its
   repository and its task. Take the rest from the board (`fleet board ls`):
   the task meant for it and that task's repository. If the board does not
   say, ask me, and do nothing else.
2. Run `fleet spawn <name> --repo <repo> --task <task> --host desktop`. It
   puts the agent on the board and prints the arguments for `start_session`.
3. Call `mcp__ccd_session__start_session` with exactly those arguments.
   If the app says I must trust the repository first, tell me, and call it
   again after I click Trust.
4. Record the `session_id` it returns:
   `fleet board agent <name> --target desktop:<session_id>`.

Then tell me in one line that it started, and end your turn.
