"""Runs one installed plugin the way the agent it was installed for runs it.

The plugin is not a script anybody executes: the agent imports it and calls what
it registered, so the only way to find out what it hands over is to be the agent
for a moment. This puts the smallest possible imitation of that agent's plugin
interface in front of the module — the shape it is documented to hand a plugin,
and no more of it than the plugin reads — and fires the callback that reports a
session.

Nothing is asserted here. What the plugin hands over is recorded by the command
it hands it to, and what that recording means is decided by the test that ran
this: a driver that judged its own output would be a driver whose idea of a
correct payload is the thing being tested.

The rules the plugin is held to are the rules here, because breaking one of them
here would look exactly like the plugin breaking it: nothing is written to
standard output, and this exits without a failure of its own. Anything worth
saying goes to standard error.

    python3 plugins.py <shape> <the installed file> <a session id>
"""

import importlib
import os
import sys


class Registrar:
    """What the agent hands a plugin to register its callbacks through.

    It offers one method, which is the one the plugin calls. What it gets back
    is remembered under the name it was registered for, because the agent calls
    a callback by firing the thing it named rather than by handing it back.
    """

    def __init__(self):
        self.hooks = {}

    def register_hook(self, event, callback):
        self.hooks[event] = callback


def load(module_file):
    """The installed module, imported by the name its directory gives it.

    Imported rather than read, and by name rather than by path, because that is
    what the agent does: the directory is a package, its name is what the
    agent's configuration switches on, and a module loaded some other way would
    be one this proves nothing about.
    """
    package = os.path.dirname(module_file)
    sys.path.insert(0, os.path.dirname(package))
    return importlib.import_module(os.path.basename(package))


def plugin_callbacks(module, session):
    """The interface Hermes loads a plugin through.

    It calls `register` once with something to register callbacks through, and
    calls each callback with keyword arguments — among them the session, and the
    interface that session is running under.
    """
    registrar = Registrar()
    module.register(registrar)
    started = registrar.hooks.get("on_session_start")
    if started is None:
        sys.exit("the plugin registered nothing that starts a session")
    started(session_id=session, platform="cli")


SHAPES = {"plugin-callbacks": plugin_callbacks}


def main(argv):
    shape, module_file, session = argv[1], argv[2], argv[3]
    drive = SHAPES.get(shape)
    if drive is None:
        sys.exit("no imitation of " + shape)
    sys.stderr.write("driving {} as {}\n".format(module_file, shape))
    drive(load(module_file), session)


if __name__ == "__main__":
    main(sys.argv)
