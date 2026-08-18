# Recorded hook payloads

One directory per agent, named the way the agent is named on `agentbus emit
--agent`. Inside it are the payloads that agent's hooks deliver on stdin, one
file each, and a `sequence.txt` describing a session built out of them.

The tests replay every directory found here through the real client into a real
daemon. Nothing in them names an agent: an agent is added to the replay by adding
its directory, and removed by removing it.

## `sequence.txt`

One line per step — the payload file to send, and the status the bus should
report about the session once it has been sent:

```
SessionStart.json      starting
UserPromptSubmit.json  working
```

The file describes one whole session rather than a list of independent cases, so
the order is part of what is being tested. Every step is expected to produce an
event; a status of `?` replays the step and asserts nothing about it, which is
what a recording that has not yet been checked against a real session uses. Blank
lines and lines beginning with `#` are ignored.
