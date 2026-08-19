#!/usr/bin/env python3
"""Turn a recorded bus stream into hook payload fixtures.

The payloads an agent's hooks deliver are the one thing in this repository that
cannot be written correctly from documentation: the documentation is what the
synthetic fixtures were written from, and the whole point of capturing is to find
out where it is wrong. This takes a recording of a real session and writes the
payloads out one file per event, in the layout the adapter tests replay.

Recording needs nothing but the bus itself, because every event carries the
payload it was normalized from:

    agentbus subscribe > session.ndjson       # in one terminal, before the session
    <drive a real session in another>
    scripts/capture-fixtures.py codex session.ndjson

Capturing this way costs the session nothing and cannot perturb it: the recorder
is an ordinary subscriber reading a socket, not a second hook in the agent's
critical path.

Nothing is overwritten without being asked. A run says what it would write; `-f`
makes it write.
"""

import argparse
import json
import os
import re
import socket
import sys
from pathlib import Path

# Where the fixtures the adapter tests replay live, relative to the repository
# root. The tests find them by the same layout: one directory per agent, named
# the way `agentbus emit --agent` names it.
FIXTURES = Path("crates/cli/tests/fixtures/hooks")

# The file in each directory that says which payloads to replay, in what order,
# and what the bus should say after each one.
SEQUENCE = "sequence.txt"

# What the sequence file's status column says where nobody has decided yet.
UNKNOWN = "?"

# What a scrubbed home directory, user and host are called instead. Invented
# names, chosen to look like what they replace so that a fixture still reads as a
# plausible payload.
PLACEHOLDER_HOME = "/home/dev"
PLACEHOLDER_USER = "dev"
PLACEHOLDER_HOST = "workstation"

# The payload fields that name the event, tried in this order. Every supported
# agent spells it one of these two ways; an agent that spells it a third way
# needs a line here and nothing else.
EVENT_NAME_FIELDS = ("hook_event_name", "type")

# A field whose value, when there is one, is appended to the file name. Claude's
# `Notification` is several different events wearing one name, and a directory
# that held only the last one captured would hide exactly the distinction the
# adapter has to make.
QUALIFIER_FIELD = "notification_type"

# Fields whose values are replaced wholesale rather than scrubbed, because they
# point at somebody's conversation rather than merely mentioning their machine.
REDACTED_FIELDS = ("transcript_path",)


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="A recording is made with: agentbus subscribe > session.ndjson",
    )
    parser.add_argument("agent", help="the agent whose payloads to take, e.g. codex")
    parser.add_argument(
        "stream",
        help="a file of subscribe output, or - to read it from standard input",
    )
    parser.add_argument(
        "-o",
        "--out",
        type=Path,
        help=f"where to write (default {FIXTURES}/<agent>)",
    )
    parser.add_argument(
        "-f",
        "--force",
        action="store_true",
        help="write the files; without this, only say what would be written",
    )
    parser.add_argument(
        "--home",
        default=os.path.expanduser("~"),
        help="the home directory to scrub out of the payloads (default this one)",
    )
    parser.add_argument(
        "--user",
        default=os.environ.get("USER", ""),
        help="the user name to scrub out of the payloads (default this one)",
    )
    parser.add_argument(
        "--host",
        default=socket.gethostname(),
        help="the host name to scrub out of the payloads (default this one)",
    )
    return parser.parse_args(argv)


def read_stream(source):
    """Every line of a recording, parsed, skipping what is not a line of one.

    A recording is written while a session is running and may well be cut off in
    the middle of a line by whoever stops it, so a line that does not parse is
    dropped with a word about it rather than being treated as a failure.
    """
    handle = sys.stdin if source == "-" else open(source, encoding="utf-8")
    with handle:
        for number, text in enumerate(handle, start=1):
            text = text.strip()
            if not text:
                continue
            try:
                yield json.loads(text)
            except json.JSONDecodeError as error:
                warn(f"line {number} is not JSON and was skipped: {error}")


def payloads(lines, agent):
    """The raw payloads for one agent, in the order they were recorded.

    Only event lines carry one. The snapshot that opens every subscription, the
    heartbeats and anything else the daemon has learned to say are skipped by
    the same rule: no `raw`, nothing to capture.
    """
    for line in lines:
        if not isinstance(line, dict) or line.get("agent") != agent:
            continue
        raw = line.get("raw")
        if isinstance(raw, dict):
            yield raw


def name_of(payload):
    """What to call the fixture holding `payload`, without its extension.

    `None` where the payload does not say what event it is, which is a payload
    the adapter would have dropped too.
    """
    for field in EVENT_NAME_FIELDS:
        name = payload.get(field)
        if isinstance(name, str) and name:
            qualifier = payload.get(QUALIFIER_FIELD)
            if isinstance(qualifier, str) and qualifier:
                return f"{name}-{qualifier}"
            return name
    return None


def scrub(value, substitutions):
    """`value` with everything personal about this machine taken out of it.

    Applied to every string at every depth rather than to a list of fields that
    are known to hold paths: a payload is somebody else's schema, it grows
    fields between releases, and a fixture is about to be committed to a public
    repository. The structure and the field names are left exactly as they
    arrived, because those are the whole of what the fixture is for.
    """
    if isinstance(value, dict):
        return {
            key: "<redacted>" if key in REDACTED_FIELDS else scrub(item, substitutions)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [scrub(item, substitutions) for item in value]
    if isinstance(value, str):
        for pattern, replacement in substitutions:
            value = pattern.sub(replacement, value)
        return value
    return value


def substitutions(home, user, host):
    """What scrubbing replaces, longest first.

    Order matters: a home directory normally has the user name inside it, and
    replacing the user name first would leave a path that no longer matches the
    home directory it came from.
    """
    wanted = [
        (home, PLACEHOLDER_HOME),
        (host, PLACEHOLDER_HOST),
        (user, PLACEHOLDER_USER),
    ]
    return [
        (re.compile(re.escape(text)), replacement)
        for text, replacement in sorted(wanted, key=lambda pair: -len(pair[0]))
        if text
    ]


def warn(message):
    print(f"capture-fixtures: {message}", file=sys.stderr)


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    out = args.out or FIXTURES / args.agent
    rules = substitutions(args.home, args.user, args.host)

    # The last payload of each event wins, and the order is the order each event
    # was first seen. A session reaches most events more than once, and the last
    # one is the one from a session that had got going rather than from its
    # first confused moments.
    captured = {}
    for payload in payloads(read_stream(args.stream), args.agent):
        name = name_of(payload)
        if name is None:
            warn(f"a payload naming no event was skipped: {json.dumps(payload)[:120]}")
            continue
        captured[name] = scrub(payload, rules)

    if not captured:
        warn(f"nothing for {args.agent} in that recording")
        return 1

    if args.force:
        out.mkdir(parents=True, exist_ok=True)
    for name, payload in captured.items():
        path = out / f"{name}.json"
        if not args.force:
            print(f"would write {path}")
            continue
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {path}")

    sequence = out / f"{SEQUENCE}.captured"
    lines = [
        "# The events this recording reached, in the order it reached them. The",
        "# status column is what the bus should say about the session after each",
        f"# one; every {UNKNOWN} needs replacing with what was actually observed",
        f"# before this becomes the {SEQUENCE} the tests read.",
    ]
    names = [f"{name}.json" for name in captured]
    width = max(len(name) for name in names) + 2
    lines += [f"{name:<{width}}{UNKNOWN}" for name in names]
    text = "\n".join(lines) + "\n"
    if args.force:
        sequence.write_text(text, encoding="utf-8")
        print(f"wrote {sequence}")
    else:
        print(f"would write {sequence}:\n{text}")

    if not args.force:
        sys.stdout.flush()
        warn("nothing was written; pass -f to write it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
