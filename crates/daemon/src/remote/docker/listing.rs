//! What `docker ps` printed, read back.
//!
//! The format is tab-separated on purpose, and it is worth saying why, because
//! the obvious alternative looks better and is not. Asking Docker for JSON
//! gives every label flattened into one comma-joined string, which cannot be
//! split back into labels once any value contains a comma — and the label this
//! reads holds a filesystem path, which may. Tabs have the same problem in
//! principle and not in practice: a path with a tab in it is possible, a path
//! with a comma in it is ordinary.
//!
//! Everything here is forgiving. A line that does not have the fields it should
//! is skipped rather than fatal, and a state this does not recognize is carried
//! as the word it was: a later Docker may print states nobody has thought of,
//! and the only one that has to be understood is the one that means the
//! container is up.

/// Which containers are asked about: the ones carrying the label the
/// devcontainer tooling puts on everything it builds, whose value is the host
/// path of the project the container was built for.
const FILTER: &str = "label=devcontainer.local_folder";

/// What is asked of each container, in the order the fields come back.
pub const FORMAT: &str =
    "{{.State}}\t{{.Label \"devcontainer.local_folder\"}}\t{{.Names}}\t{{.ID}}";

/// The state of a container that is up.
const RUNNING: &str = "running";

/// What separates one field from the next.
const FIELD: char = '\t';

/// What separates one of a container's names from the next, where it has
/// several. A container name cannot contain one, so this never splits a name.
const NAMES: char = ',';

/// How many fields a usable line has.
const FIELDS: usize = 4;

/// The whole command, as it is sent.
///
/// `-a` rather than only the running ones: a container that has stopped is
/// still the answer to "is this project's container up", and knowing it exists
/// and is down is different from not knowing about it.
pub fn command() -> [&'static str; 6] {
    ["ps", "-a", "--filter", FILTER, "--format", FORMAT]
}

/// One container, as `docker ps` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The word Docker used for what it is doing: `running`, `exited`, …
    pub state: String,
    /// The host path of the project it was built for.
    pub folder: String,
    /// What it is called.
    pub name: String,
    /// Its id, as short as Docker chose to print it.
    pub id: String,
}

impl Listed {
    /// Whether it is up, and so worth trying to reach.
    pub fn running(&self) -> bool {
        self.state == RUNNING
    }
}

/// Every container in what `docker ps` printed, in the order it printed them.
pub fn listed(printed: &str) -> Vec<Listed> {
    printed.lines().filter_map(one).collect()
}

/// One line, or nothing for a line that is not one.
fn one(line: &str) -> Option<Listed> {
    let fields: Vec<&str> = line.split(FIELD).collect();
    if fields.len() != FIELDS {
        return None;
    }
    let name = fields[2].split(NAMES).next().unwrap_or_default().trim();
    let id = fields[3].trim();
    if name.is_empty() || id.is_empty() {
        return None;
    }
    Some(Listed {
        state: fields[0].trim().to_owned(),
        // Not trimmed: it is a path, and a path is whatever it is. The others
        // are words Docker chose and cannot begin or end with a space.
        folder: fields[1].to_owned(),
        name: name.to_owned(),
        id: id.to_owned(),
    })
}
