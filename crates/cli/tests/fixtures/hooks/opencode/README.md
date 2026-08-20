# OpenCode plugin payloads

One file per event, named after the event's `type`. Each is a whole payload as
the installed plugin writes it to the emit command's stdin: the event's `type`,
the `directory` the plugin was loaded for, and the event's own properties spread
out beside them. OpenCode has no hook commands, so there is no payload of the
agent's own to record — this shape is the plugin's.

Two files are here for something other than the event they name: one records the
second spelling of the session id, with a `-suffix` saying so, and one records an
event nothing normalizes, so that it must go on producing nothing.

**These are synthetic.** They were written by hand from OpenCode's documented
event list, not captured from a running session, so the *shape* is the
documented one but the values are invented and the properties are only the ones
the documentation names. They are placeholders: replacing them with payloads
captured from real sessions is the point, and the tests are written so that
doing so needs no change to the tests — they assert the normalized kind and
detail, and nothing about the incidental fields around them.

A session id is shared across the files, and a permission id across the two
events that belong to one permission, so that the directory reads as one
plausible session from start to end.

There is no payload for the start of a turn, because OpenCode has no event that
means one. That is a gap in the agent, not a gap in this directory.
