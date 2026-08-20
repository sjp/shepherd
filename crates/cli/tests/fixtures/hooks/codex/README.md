# Codex CLI hook payloads

One file per hook event, named after the event as Codex spells it in
`hook_event_name`. Each is a whole payload as it would arrive on the hook
command's stdin. Not all of them produce an event: an event another agent has
and this one does not is recorded here too, so that answering for it would be a
failure rather than a silence.

**These are synthetic.** They were written by hand from Codex's documented hook
surface, not captured from a running session, so the field *shapes* are the
documented ones but the values are invented and the set of fields is only what
the documentation names. They are placeholders: replacing them with payloads
captured from real sessions is the point, and the tests are written so that
doing so needs no change to the tests — they assert the normalized kind and
detail, and nothing about the incidental fields around them.

A session id is shared across the files, and a turn id across the events that
belong to one turn, so that the directory reads as one plausible session from
start to end.
