# Captured screens

Real terminal screens from real agent sessions, kept so that a change in an
agent's interface fails a test here rather than going unnoticed until someone's
pane sits on the wrong status.

Every file in this tree was captured from a live session. Nothing here is
written by hand: a synthetic screen proves only that the engine agrees with
whoever invented it, which is what the unit tests already cover.

## Layout

```
screens/<agent>/<case>.txt         the screen, escape sequences already removed
screens/<agent>/<case>.expected    what it should be read as
screens/<agent>/<case>.title       the OSC title at capture time, when there was one
```

`<agent>` is the manifest id — `claude`, `codex`, `opencode` — and `<case>`
names the situation: `working`, `idle`, `blocked-bash`, `transcript`. A `.txt`
with no `.expected` beside it fails the replay: an unlabelled capture is a
capture nobody checked.

## The `.expected` sidecar

One line. The first word is the state — `working`, `idle`, `blocked` or
`unknown`. After it come the markers that should be set, in any order:

- `visible` — the state's chrome is live on the screen right now, not merely
  mentioned by text the agent happens to be showing.
- `skip` — the screen describes the past rather than the present, so a reader
  should keep whatever it believed before instead of adopting this verdict.

Absent markers must be absent in the verdict too, so a sidecar reading
`blocked` asserts *not* visible just as firmly as `blocked visible` asserts
visible. Blank lines and lines starting `#` are ignored, which is where a note
about what the session was doing goes.

```
# permission prompt for `rm -rf build/`, answered No
blocked visible
```

## Capturing

`scripts/capture-screen.sh` takes the capture and writes the pair. It strips
escape sequences and records the OSC title where the terminal offers one; what
it cannot do is know what the screen meant, so it leaves the `.expected` line
for the person who was watching.

Before committing a capture, read it. These files are published, and a terminal
holds whatever was on it — absolute paths, host names, ticket numbers, a token
echoed into a prompt. Edit those out. What must survive the edit is the chrome
the rules match: box drawing, key hints, prompt wording, the shape of the
menu. Replacing a path with a shorter path is fine; deleting the line it sat on
may take a rule's landmark with it.
