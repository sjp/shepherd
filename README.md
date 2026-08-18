# agentbus and Shepherd

**`agentbus`** is a generic event bus for coding agents. Agents report what they
are doing through hooks; the daemon folds those events into the current status of
each session and publishes them to any subscriber over a Unix socket, including
sessions running inside containers or on remote hosts. It is a single small
binary, useful on its own from a shell script or a terminal.

**Shepherd** is a GUI terminal multiplexer that consumes the bus: panes, tabs and
workspaces that show at a glance which agent is waiting for you. It is a separate
application and a later deliverable; `agentbus` neither knows nor cares that it
exists.

Technology choices and the reasoning behind them are recorded in
[`docs/decisions.md`](docs/decisions.md).
