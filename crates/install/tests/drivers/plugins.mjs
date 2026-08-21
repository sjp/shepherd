// Runs one installed plugin the way the agent it was installed for runs it.
//
// The plugins are not scripts anybody executes: an agent loads them and calls
// what they export, so the only way to find out what one hands over is to be
// the agent for a moment. This puts the smallest possible imitation of each
// agent's plugin interface in front of the file — the shape that interface is
// documented to hand a plugin, and no more of it than the plugin reads — and
// fires the callback that reports a session.
//
// Nothing is asserted here. What the plugin hands over is recorded by the
// command it hands it to, and what that recording means is decided by the test
// that ran this: a driver that judged its own output would be a driver whose
// idea of a correct payload is the thing being tested.
//
// The rules the plugins are held to are the rules here, because breaking one of
// them here would look exactly like the plugin breaking it: nothing is written
// to standard output, and this exits without a failure of its own. Anything
// worth saying goes to standard error.
//
//     node plugins.mjs <shape> <the installed file> <a session id>

import { copyFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

// Loads the installed file as the module it is.
//
// Node decides how to read a file from its name and from the nearest
// package.json, and neither says anything true about a file written into
// somebody's plugin directory: the agents load these with loaders of their own,
// one of which reads TypeScript. So the file is copied — byte for byte, under a
// name node reads as a module — and the copy is what gets loaded. The copy sits
// beside the original and is left there, because the directory it is in belongs
// to a test that is about to throw it away.
async function load(file) {
  const loadable = `${file.replace(/\.[^./]+$/, "")}.as-loaded.mjs`;
  copyFileSync(file, loadable);
  return import(pathToFileURL(loadable).href);
}

// The imitations, one per plugin interface rather than one per agent: two
// agents reading the same interface get the same fake, which is what makes them
// the same interface.
const SHAPES = {
  // The interface OpenCode and Kilo share. It calls what the plugin exports
  // with the directory the plugin was loaded for and gets back an object of
  // callbacks, one of which is handed every event the agent produces. An event
  // is a name and a bag of properties.
  "plugin-event": async (module, session) => {
    const plugin = await module.AgentBus({ directory: process.cwd() });
    await plugin.event({
      event: { type: "session.created", properties: { sessionID: session } },
    });
  },

  // The interface OpenCode's terminal loads a plugin through. It produces no
  // events: what it offers is where the user is — a route, and the session that
  // route names — and the plugin reads it whenever it likes. It is asked here
  // for a session that is a session of its own rather than one the agent
  // started underneath another, because that is the case the plugin reports.
  "tui-session": async (module, session) => {
    let dispose;
    const api = {
      route: { current: { name: "session", params: { sessionID: session } } },
      state: { session: { get: (id) => ({ id, parentID: undefined }) } },
      lifecycle: { onDispose: (callback) => (dispose = callback) },
    };

    await module.default.tui(api);
    // The plugin watches on a timer, so it is told the interface has gone the
    // way the interface would tell it. Without this the timer is still armed
    // when this process would otherwise be finished with it.
    dispose?.();
  },

  // The interface Pi and Omp share. It calls what the extension exports once,
  // with something to subscribe to the agent's events through, and calls each
  // subscriber with the event that fired and the context it fired in. The
  // session is read back out of that context rather than carried in the event.
  "extension-subscribe": async (module, session) => {
    const subscribers = new Map();
    const agent = { on: (event, handler) => subscribers.set(event, handler) };
    const context = {
      sessionManager: { getSessionId: () => session },
      // Both spellings of what interface this is, because the two agents that
      // read this shape report it differently and each takes the one it knows.
      mode: "tui",
      hasUI: true,
    };

    await module.default(agent);
    const started = subscribers.get("session_start");
    if (!started) {
      throw new Error("the extension subscribed to nothing that starts a session");
    }
    await started({ type: "session_start" }, context);
  },
};

const [shape, file, session] = process.argv.slice(2);
const drive = SHAPES[shape];
if (!drive) {
  throw new Error(`no imitation of ${shape}`);
}

process.stderr.write(`driving ${file} as ${shape}\n`);
await drive(await load(file), session);
