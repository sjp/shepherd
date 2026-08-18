# Claude Code hook payloads

One file per hook event the adapter maps, named after the event as Claude spells
it in `hook_event_name`. Each is a whole payload as it would arrive on the hook
command's stdin.

**These are synthetic.** They were written by hand from Claude Code's documented
hook schema, not captured from a running session, so the field *shapes* are the
documented ones but the values are invented and the set of fields is only what
the documentation names. They are placeholders: replacing them with payloads
captured from real sessions is the point, and the tests are written so that doing
so needs no change to the tests — they assert the normalized kind and detail, and
nothing about the incidental fields around them.

A session id is shared across the files so that the directory reads as one
plausible session from start to end.
