//! How the shells in one tab are arranged, and what "the one to the left"
//! means.
//!
//! A tab holds a tree. Its leaves are shells; its interior nodes are splits,
//! each of which arranges two or more children along one axis and gives each of
//! them a fraction of the space it was given. There is no separate concept
//! sitting between a shell and its place in the tree: a leaf *is* a shell's
//! slot, so a tab with one shell and a tab with a dozen are the same type in
//! the same shape, and nothing has to be special-cased for the tab that has not
//! been split yet.
//!
//! # Two invariants, and what they buy
//!
//! **A split has at least two children.** A split with one child is the child,
//! so closing a shell collapses whatever it leaves behind rather than leaving a
//! node that divides nothing.
//!
//! **No split has a child split on its own axis.** Splitting a shell to the
//! right when it already sits in a row inserts a third column into that row,
//! rather than nesting a two-column row inside one of its columns. The two
//! arrangements look identical on screen, so allowing both would mean the same
//! picture had many representations — and every question asked of the tree
//! would have to be answered for all of them. Splitting maintains this by
//! inserting a sibling where it can, and closing maintains it by absorbing a
//! collapsed child back into its parent.
//!
//! # Why the geometry is here rather than in the renderer
//!
//! "Move focus left" cannot be answered by walking parents and children. In an
//! arrangement of one tall shell beside two stacked ones, the tall shell's
//! neighbour to the right is whichever of the two the eye would pick, and which
//! one that is depends on where they sit — information a parent/child walk does
//! not have. So this module lays the tree out in a unit square and answers the
//! question against the rectangles, which is the same thing the renderer does
//! at a larger scale. Getting that wrong is invisible until there is a window
//! open, and awkward to correct once one is.

use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;

use crate::ids::ShellId;

#[cfg(test)]
mod tests;

/// How far apart two edges may be and still count as touching.
///
/// The layout is computed in a unit square from fractions that have been
/// halved and rescaled, so shared edges agree to within rounding error rather
/// than exactly.
const ADJACENCY: f32 = 1e-4;

/// How far a split's shares may be from summing to one before
/// [`SplitTree::split_of`] rescales them.
///
/// Shares that already sum to one are left exactly as they are, so that a tree
/// taken apart and put back together is the tree it was — dividing every share
/// by a total of `0.99999994` would not be. What the tolerance is for is the
/// arrangement somebody wrote by hand, where `1` and `1` plainly means half
/// each; a difference this small cannot move an edge by a whole pixel on any
/// window anybody has.
const SHARES_TOLERANCE: f32 = 1e-3;

/// The axis a split arranges its children along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    /// Children sit side by side, first one leftmost.
    Horizontal,
    /// Children sit one above the other, first one topmost.
    Vertical,
}

impl Axis {
    /// The other axis: the one a split's children all span in full.
    pub fn perpendicular(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        })
    }
}

/// A direction on screen, as a person pressing an arrow key means it.
///
/// One enum covers splitting and moving focus because both questions are asked
/// the same way — "to the right of this shell" is where a new shell goes and
/// where the next one to focus is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Towards smaller x.
    Left,
    /// Towards larger x.
    Right,
    /// Towards smaller y.
    Up,
    /// Towards larger y.
    Down,
}

impl Direction {
    /// The axis this direction runs along.
    pub fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Up | Self::Down => Axis::Vertical,
        }
    }

    /// Whether this direction runs towards larger coordinates, which is also
    /// the order children are stored in.
    pub fn is_forward(self) -> bool {
        matches!(self, Self::Right | Self::Down)
    }
}

/// A rectangle, in whatever coordinates it was laid out in.
///
/// Laying a tree out on its own uses a unit square, so a rectangle is a
/// fraction of the tab. Laying it out in a window's bounds gives the same
/// rectangles at that window's scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Distance from the left edge to this rectangle's left edge.
    pub x: f32,
    /// Distance from the top edge to this rectangle's top edge.
    pub y: f32,
    /// Extent along the horizontal axis.
    pub width: f32,
    /// Extent along the vertical axis.
    pub height: f32,
}

impl Rect {
    /// The whole of the space, which is what a tab is laid out in when nothing
    /// says otherwise.
    pub const UNIT: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    /// A rectangle at `(x, y)` of the given size.
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The near edge along `axis`.
    pub fn start(self, axis: Axis) -> f32 {
        match axis {
            Axis::Horizontal => self.x,
            Axis::Vertical => self.y,
        }
    }

    /// The extent along `axis`.
    pub fn extent(self, axis: Axis) -> f32 {
        match axis {
            Axis::Horizontal => self.width,
            Axis::Vertical => self.height,
        }
    }

    /// The far edge along `axis`.
    pub fn end(self, axis: Axis) -> f32 {
        self.start(axis) + self.extent(axis)
    }

    /// The midpoint along `axis`.
    pub fn midpoint(self, axis: Axis) -> f32 {
        self.start(axis) + self.extent(axis) / 2.0
    }

    /// The part of this rectangle `extent` long, starting `offset` from its
    /// near edge along `axis` and spanning it in full across that axis.
    fn slice(self, axis: Axis, offset: f32, extent: f32) -> Self {
        match axis {
            Axis::Horizontal => Self::new(self.x + offset, self.y, extent, self.height),
            Axis::Vertical => Self::new(self.x, self.y + offset, self.width, extent),
        }
    }
}

/// One shell and the rectangle it occupies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedShell {
    /// The shell.
    pub shell: ShellId,
    /// Where it sits.
    pub bounds: Rect,
}

/// One child of a split, and how much of the split's space it takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    size: f32,
    tree: SplitTree,
}

impl Branch {
    /// A child taking `size` of its split's space.
    ///
    /// Nothing is checked here, because a share only means anything alongside
    /// its siblings: whether it is one this module would build is decided by
    /// [`SplitTree::split_of`], which can see all of them.
    pub fn new(size: f32, tree: SplitTree) -> Self {
        Self { size, tree }
    }

    /// This child's share of its split's space, as a fraction. The shares of
    /// one split's children sum to one.
    pub fn size(&self) -> f32 {
        self.size
    }

    /// What is in this child.
    pub fn tree(&self) -> &SplitTree {
        &self.tree
    }
}

/// Two or more subtrees laid out along one axis.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    axis: Axis,
    children: Vec<Branch>,
}

impl Split {
    /// The axis the children are arranged along.
    pub fn axis(&self) -> Axis {
        self.axis
    }

    /// The children, in the order they appear along the axis.
    pub fn children(&self) -> &[Branch] {
        &self.children
    }

    /// Splits a leaf somewhere beneath this split. See [`SplitTree::split`].
    fn split(&mut self, target: ShellId, direction: Direction, fresh: ShellId) -> bool {
        if self.axis == direction.axis() {
            if let Some(index) = self
                .children
                .iter()
                .position(|b| b.tree.shell() == Some(target))
            {
                // The target already lies along this axis, so the new shell
                // joins the row rather than starting one of its own inside it.
                let share = self.children[index].size / 2.0;
                self.children[index].size = share;
                let at = if direction.is_forward() {
                    index + 1
                } else {
                    index
                };
                self.children.insert(
                    at,
                    Branch {
                        size: share,
                        tree: SplitTree::Leaf(fresh),
                    },
                );
                return true;
            }
        }
        self.children
            .iter_mut()
            .any(|branch| branch.tree.split(target, direction, fresh))
    }

    /// Removes a leaf from somewhere beneath this split, leaving the split with
    /// one fewer child if the leaf was one of its own. Collapsing a split that
    /// is left with a single child is the caller's job, since only the caller
    /// holds the node the collapsed child has to replace.
    fn close(&mut self, shell: ShellId) -> Closed {
        if let Some(index) = self
            .children
            .iter()
            .position(|b| b.tree.shell() == Some(shell))
        {
            let successor = self.successor_of(index);
            self.children.remove(index);
            self.rescale();
            return Closed::Removed { successor };
        }
        for index in 0..self.children.len() {
            match self.children[index].tree.close(shell) {
                Closed::Removed { successor } => {
                    self.absorb(index);
                    return Closed::Removed { successor };
                }
                // A child leaf naming this shell was dealt with above, so the
                // only thing left for a child to say is that it does not hold
                // it.
                Closed::Emptied | Closed::NotFound => {}
            }
        }
        Closed::NotFound
    }

    /// Which shell should take focus when the child at `index` goes: the one
    /// nearest to it, preferring the side the remaining space comes from.
    fn successor_of(&self, index: usize) -> ShellId {
        if index > 0 {
            self.children[index - 1].tree.last_shell()
        } else {
            self.children[1].tree.first_shell()
        }
    }

    /// Restores the invariant that the children's shares sum to one, after a
    /// child has been removed or inserted.
    fn rescale(&mut self) {
        let total: f32 = self.children.iter().map(|branch| branch.size).sum();
        let even = 1.0 / self.children.len() as f32;
        for branch in &mut self.children {
            branch.size = if total > 0.0 {
                branch.size / total
            } else {
                even
            };
        }
    }

    /// Splices the child at `index` into this split if collapsing left it a
    /// split on this same axis, which is the one way the no-nested-same-axis
    /// invariant can be broken from below.
    fn absorb(&mut self, index: usize) {
        let nested = match &self.children[index].tree {
            SplitTree::Split(inner) => inner.axis == self.axis,
            SplitTree::Leaf(_) => false,
        };
        if !nested {
            return;
        }
        let slot = self.children[index].size;
        let branch = self.children.remove(index);
        let SplitTree::Split(inner) = branch.tree else {
            return;
        };
        for (offset, mut child) in inner.children.into_iter().enumerate() {
            child.size *= slot;
            self.children.insert(index + offset, child);
        }
    }
}

/// Why an arrangement is not one this module will build. See
/// [`SplitTree::split_of`].
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum MalformedSplit {
    /// A split dividing its space between fewer than two children, which is
    /// the child rather than a split.
    #[error("a split divides its space between at least two children, and this one has {0}")]
    TooFewChildren(usize),
    /// A split holding another split on its own axis: the same picture written
    /// two ways, which is the one thing the tree's shape rules out.
    #[error("a {0} split holds another one, which is the same arrangement written twice")]
    NestedAxis(Axis),
    /// A share that is not a fraction of anything.
    #[error("a child's share of a split is {0}, and a share is a positive fraction")]
    Share(f32),
    /// One shell in two places at once.
    #[error("shell {0} is in the arrangement more than once")]
    DuplicateShell(ShellId),
}

/// What closing a shell did to the tree it was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// The tree does not hold that shell, and nothing changed.
    NotFound,
    /// The shell was the tab's last one. A tree always has at least one leaf,
    /// so nothing was removed: it is the tab that has to go, which is a
    /// decision above this tree's level.
    Emptied,
    /// The shell is gone and the space it had went to its neighbours.
    Removed {
        /// The shell nearest to where the closed one was, which is where focus
        /// should land if the closed one had it.
        successor: ShellId,
    },
}

/// The shells in one tab and how they are arranged.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitTree {
    /// One shell, filling whatever space this part of the tree was given.
    Leaf(ShellId),
    /// Space divided between two or more subtrees.
    Split(Split),
}

impl SplitTree {
    /// A tree of one shell, which is what a new tab starts as.
    pub fn leaf(shell: ShellId) -> Self {
        Self::Leaf(shell)
    }

    /// `children` divided along `axis`, for an arrangement being rebuilt from
    /// somewhere else rather than grown by splitting.
    ///
    /// Splitting and closing maintain this module's invariants step by step;
    /// something arriving whole — a layout read back off disk, which is the
    /// only way to get here — has to be checked against them instead, because
    /// nothing about the file it came out of stops it describing an
    /// arrangement that could never have been built. What is checked is
    /// exactly what the invariants say: at least two children, no child split
    /// on this same axis, a positive share each, and no shell appearing twice.
    ///
    /// Shares that do not sum to one are rescaled so that they do; shares that
    /// already do are untouched. See [`SHARES_TOLERANCE`].
    pub fn split_of(axis: Axis, children: Vec<Branch>) -> Result<Self, MalformedSplit> {
        if children.len() < 2 {
            return Err(MalformedSplit::TooFewChildren(children.len()));
        }

        let mut seen = BTreeSet::new();
        for branch in &children {
            if !branch.size.is_finite() || branch.size <= 0.0 {
                return Err(MalformedSplit::Share(branch.size));
            }
            if let Self::Split(inner) = &branch.tree
                && inner.axis == axis
            {
                return Err(MalformedSplit::NestedAxis(axis));
            }
            for shell in branch.tree.shells() {
                if !seen.insert(shell) {
                    return Err(MalformedSplit::DuplicateShell(shell));
                }
            }
        }

        let mut split = Split { axis, children };
        let total: f32 = split.children.iter().map(Branch::size).sum();
        if (total - 1.0).abs() > SHARES_TOLERANCE {
            split.rescale();
        }
        Ok(Self::Split(split))
    }

    /// The shell here, if this is a leaf rather than a split.
    pub fn shell(&self) -> Option<ShellId> {
        match self {
            Self::Leaf(shell) => Some(*shell),
            Self::Split(_) => None,
        }
    }

    /// Whether `shell` is somewhere in this tree.
    pub fn contains(&self, shell: ShellId) -> bool {
        match self {
            Self::Leaf(here) => *here == shell,
            Self::Split(split) => split
                .children
                .iter()
                .any(|branch| branch.tree.contains(shell)),
        }
    }

    /// Every shell in the tree, in the order they are arranged — left to right
    /// and top to bottom, at every depth.
    ///
    /// This is how anything that has to touch all of them at once finds them:
    /// a shell that is nested three splits deep in a tab nobody is looking at
    /// is still a running process whose output still has to be read.
    pub fn shells(&self) -> Vec<ShellId> {
        let mut found = Vec::new();
        self.collect_shells(&mut found);
        found
    }

    fn collect_shells(&self, found: &mut Vec<ShellId>) {
        match self {
            Self::Leaf(shell) => found.push(*shell),
            Self::Split(split) => {
                for branch in &split.children {
                    branch.tree.collect_shells(found);
                }
            }
        }
    }

    /// The first shell in arrangement order. A tree always has one.
    pub fn first_shell(&self) -> ShellId {
        match self {
            Self::Leaf(shell) => *shell,
            Self::Split(split) => split.children[0].tree.first_shell(),
        }
    }

    /// The last shell in arrangement order. A tree always has one.
    pub fn last_shell(&self) -> ShellId {
        match self {
            Self::Leaf(shell) => *shell,
            Self::Split(split) => split.children[split.children.len() - 1].tree.last_shell(),
        }
    }

    /// Puts `fresh` beside `target` on the given side, halving the space
    /// `target` had. Answers whether `target` was in this tree at all.
    ///
    /// The caller supplies the new shell's id rather than the tree minting one:
    /// a tree does not know which workspace's run of ids it belongs to, and
    /// making it know would be the first crack in "the tree is only shape".
    pub fn split(&mut self, target: ShellId, direction: Direction, fresh: ShellId) -> bool {
        match self {
            Self::Leaf(shell) => {
                if *shell != target {
                    return false;
                }
                let existing = Branch {
                    size: 0.5,
                    tree: Self::Leaf(target),
                };
                let added = Branch {
                    size: 0.5,
                    tree: Self::Leaf(fresh),
                };
                let children = if direction.is_forward() {
                    vec![existing, added]
                } else {
                    vec![added, existing]
                };
                *self = Self::Split(Split {
                    axis: direction.axis(),
                    children,
                });
                true
            }
            Self::Split(split) => split.split(target, direction, fresh),
        }
    }

    /// Takes `shell` out of the tree, giving its space back to its neighbours
    /// and collapsing any split it leaves with a single child.
    pub fn close(&mut self, shell: ShellId) -> Closed {
        match self {
            Self::Leaf(here) => {
                if *here == shell {
                    Closed::Emptied
                } else {
                    Closed::NotFound
                }
            }
            Self::Split(split) => {
                let outcome = split.close(shell);
                if matches!(outcome, Closed::Removed { .. }) && split.children.len() == 1 {
                    let only = split.children.remove(0);
                    *self = only.tree;
                }
                outcome
            }
        }
    }

    /// The shell a person moving focus in `direction` from `from` would expect
    /// to land on, or `None` if there is nothing that way.
    ///
    /// A candidate has to begin at or beyond the edge being crossed and to
    /// overlap `from` across that edge — meeting it only at a corner does not
    /// count. Of those the nearest wins, and among equally near ones the one
    /// the middle of the shell being left points at, which is what makes
    /// leaving a wide shell land where the eye already is. Anything still tied
    /// goes to whichever is topmost or leftmost, so that the same question
    /// always gets the same answer.
    pub fn neighbour(&self, from: ShellId, direction: Direction) -> Option<ShellId> {
        let placed = self.layout();
        let origin = placed
            .iter()
            .find(|candidate| candidate.shell == from)?
            .bounds;
        let along = direction.axis();
        let across = along.perpendicular();

        placed
            .iter()
            .enumerate()
            .filter_map(|(order, candidate)| {
                if candidate.shell == from {
                    return None;
                }
                let gap = if direction.is_forward() {
                    candidate.bounds.start(along) - origin.end(along)
                } else {
                    origin.start(along) - candidate.bounds.end(along)
                };
                if gap < -ADJACENCY {
                    return None;
                }
                let overlap = candidate.bounds.end(across).min(origin.end(across))
                    - candidate.bounds.start(across).max(origin.start(across));
                if overlap <= ADJACENCY {
                    return None;
                }
                let middle = origin.midpoint(across);
                Some(Candidate {
                    gap: gap.max(0.0),
                    aim: (candidate.bounds.start(across) - middle)
                        .max(middle - candidate.bounds.end(across))
                        .max(0.0),
                    offset: (candidate.bounds.midpoint(across) - middle).abs(),
                    across_start: candidate.bounds.start(across),
                    order,
                    shell: candidate.shell,
                })
            })
            .min_by(Candidate::compare)
            .map(|candidate| candidate.shell)
    }

    /// Every shell's place in a unit square, which is what
    /// [`SplitTree::neighbour`] reasons about.
    pub fn layout(&self) -> Vec<PlacedShell> {
        self.layout_in(Rect::UNIT)
    }

    /// Every shell's place within `bounds`, in the order [`SplitTree::shells`]
    /// gives them.
    pub fn layout_in(&self, bounds: Rect) -> Vec<PlacedShell> {
        let mut placed = Vec::new();
        self.place(bounds, &mut placed);
        placed
    }

    fn place(&self, bounds: Rect, placed: &mut Vec<PlacedShell>) {
        match self {
            Self::Leaf(shell) => placed.push(PlacedShell {
                shell: *shell,
                bounds,
            }),
            Self::Split(split) => {
                let total = bounds.extent(split.axis);
                let last = split.children.len() - 1;
                let mut offset = 0.0;
                for (index, branch) in split.children.iter().enumerate() {
                    // The final child takes what is left rather than its own
                    // share, so that rounding cannot leave a sliver of the tab
                    // belonging to nothing.
                    let extent = if index == last {
                        total - offset
                    } else {
                        branch.size * total
                    };
                    branch
                        .tree
                        .place(bounds.slice(split.axis, offset, extent), placed);
                    offset += extent;
                }
            }
        }
    }
}

/// A shell that might be the one in a given direction, and everything used to
/// decide between it and the others.
struct Candidate {
    /// How far past the edge being crossed this shell begins.
    gap: f32,
    /// How far the middle of the shell being left falls outside this one, and
    /// zero when it points straight at it.
    aim: f32,
    /// How far this shell's middle is from that same point.
    offset: f32,
    /// Where this shell begins across the direction of travel.
    across_start: f32,
    /// Where this shell came in the layout, which decides nothing except that
    /// two shells that are alike in every way above are still ordered.
    order: usize,
    shell: ShellId,
}

impl Candidate {
    fn compare(&self, other: &Self) -> std::cmp::Ordering {
        self.gap
            .total_cmp(&other.gap)
            .then(self.aim.total_cmp(&other.aim))
            .then(self.offset.total_cmp(&other.offset))
            .then(self.across_start.total_cmp(&other.across_start))
            .then(self.order.cmp(&other.order))
    }
}
