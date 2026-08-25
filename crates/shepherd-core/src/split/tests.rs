use super::*;

/// Shells are named by number in these tests, so that an arrangement can be
/// written out and read back without a workspace being involved.
fn shell(raw: u32) -> ShellId {
    ShellId::from_raw(raw)
}

/// The shell each of a tree's leaves holds, with where it sits, sorted so that
/// an assertion does not depend on the order the tree happens to walk in.
fn placed(tree: &SplitTree) -> Vec<(u32, Rect)> {
    let mut found: Vec<(u32, Rect)> = tree
        .layout()
        .into_iter()
        .map(|leaf| (leaf.shell.raw(), leaf.bounds))
        .collect();
    found.sort_by_key(|(raw, _)| *raw);
    found
}

/// Compares two numbers that were arrived at by dividing and rescaling, and so
/// agree to within rounding rather than exactly.
fn assert_close(left: f32, right: f32, what: &str) {
    let tolerance = 1e-5 * left.abs().max(right.abs()).max(1.0);
    assert!(
        (left - right).abs() <= tolerance,
        "{what}: {left} is not {right}"
    );
}

/// Two columns, the right one split into two rows: the arrangement that a
/// parent/child walk cannot navigate correctly.
///
/// ```text
/// +--------+--------+
/// |        |   1    |
/// |   0    +--------+
/// |        |   2    |
/// +--------+--------+
/// ```
fn one_beside_two() -> SplitTree {
    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Right, shell(1)));
    assert!(tree.split(shell(1), Direction::Down, shell(2)));
    tree
}

#[test]
fn a_tree_starts_as_one_shell_filling_everything() {
    let tree = SplitTree::leaf(shell(0));
    assert_eq!(tree.shells(), vec![shell(0)]);
    assert_eq!(placed(&tree), vec![(0, Rect::UNIT)]);
    assert!(tree.contains(shell(0)));
    assert!(!tree.contains(shell(1)));
}

#[test]
fn splitting_puts_the_new_shell_on_the_side_that_was_asked_for() {
    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Right, shell(1)));
    assert_eq!(tree.shells(), vec![shell(0), shell(1)]);
    assert_eq!(
        placed(&tree),
        vec![
            (0, Rect::new(0.0, 0.0, 0.5, 1.0)),
            (1, Rect::new(0.5, 0.0, 0.5, 1.0)),
        ]
    );

    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Left, shell(1)));
    assert_eq!(
        tree.shells(),
        vec![shell(1), shell(0)],
        "splitting leftwards puts the new shell first"
    );

    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Up, shell(1)));
    assert_eq!(
        placed(&tree),
        vec![
            (0, Rect::new(0.0, 0.5, 1.0, 0.5)),
            (1, Rect::new(0.0, 0.0, 1.0, 0.5)),
        ]
    );
}

#[test]
fn splitting_a_shell_that_is_not_here_changes_nothing() {
    let mut tree = one_beside_two();
    let before = tree.clone();
    assert!(!tree.split(shell(7), Direction::Right, shell(8)));
    assert_eq!(tree, before);
}

#[test]
fn splitting_along_a_row_widens_the_row_rather_than_nesting_inside_it() {
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Right, shell(1));
    tree.split(shell(1), Direction::Right, shell(2));

    let SplitTree::Split(split) = &tree else {
        panic!("splitting made a split");
    };
    assert_eq!(split.axis(), Axis::Horizontal);
    assert_eq!(
        split.children().len(),
        3,
        "a third column joined the row instead of nesting in the second"
    );
    for (index, branch) in split.children().iter().enumerate() {
        assert!(
            matches!(branch.tree(), SplitTree::Leaf(_)),
            "child {index} is a leaf"
        );
    }
    assert_close(split.children()[0].size(), 0.5, "the untouched column");
    assert_close(split.children()[1].size(), 0.25, "the halved column");
    assert_close(split.children()[2].size(), 0.25, "the new column");
}

#[test]
fn splitting_across_a_row_nests_because_it_has_to() {
    let tree = one_beside_two();
    let SplitTree::Split(split) = &tree else {
        panic!("splitting made a split");
    };
    assert_eq!(split.axis(), Axis::Horizontal);
    assert_eq!(split.children().len(), 2);
    assert_eq!(split.children()[0].tree().shell(), Some(shell(0)));
    let SplitTree::Split(inner) = split.children()[1].tree() else {
        panic!("the second column holds the two rows");
    };
    assert_eq!(inner.axis(), Axis::Vertical);
    assert_eq!(inner.children().len(), 2);
}

#[test]
fn every_shell_is_listed_however_deep_it_is() {
    let mut tree = one_beside_two();
    tree.split(shell(2), Direction::Right, shell(3));
    tree.split(shell(3), Direction::Down, shell(4));

    assert_eq!(
        tree.shells(),
        vec![shell(0), shell(1), shell(2), shell(3), shell(4)]
    );
    assert_eq!(tree.first_shell(), shell(0));
    assert_eq!(tree.last_shell(), shell(4));
    assert_eq!(tree.layout().len(), 5);
}

#[test]
fn the_laid_out_shells_tile_the_space_they_were_given() {
    let mut tree = one_beside_two();
    tree.split(shell(2), Direction::Right, shell(3));

    let bounds = Rect::new(10.0, 20.0, 640.0, 480.0);
    let placed = tree.layout_in(bounds);
    let covered: f32 = placed
        .iter()
        .map(|leaf| leaf.bounds.width * leaf.bounds.height)
        .sum();
    assert_close(covered, bounds.width * bounds.height, "the covered area");

    for leaf in &placed {
        assert!(leaf.bounds.x >= bounds.x - 1e-3);
        assert!(leaf.bounds.y >= bounds.y - 1e-3);
        assert!(leaf.bounds.end(Axis::Horizontal) <= bounds.end(Axis::Horizontal) + 1e-3);
        assert!(leaf.bounds.end(Axis::Vertical) <= bounds.end(Axis::Vertical) + 1e-3);
    }
}

#[test]
fn focus_moves_across_a_regular_grid() {
    // Four quarters: 0 1 on top, 2 3 beneath.
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Right, shell(1));
    tree.split(shell(0), Direction::Down, shell(2));
    tree.split(shell(1), Direction::Down, shell(3));

    assert_eq!(tree.neighbour(shell(0), Direction::Right), Some(shell(1)));
    assert_eq!(tree.neighbour(shell(1), Direction::Left), Some(shell(0)));
    assert_eq!(tree.neighbour(shell(0), Direction::Down), Some(shell(2)));
    assert_eq!(tree.neighbour(shell(3), Direction::Up), Some(shell(1)));
    assert_eq!(tree.neighbour(shell(2), Direction::Right), Some(shell(3)));

    assert_eq!(tree.neighbour(shell(0), Direction::Left), None);
    assert_eq!(tree.neighbour(shell(0), Direction::Up), None);
    assert_eq!(tree.neighbour(shell(3), Direction::Right), None);
    assert_eq!(tree.neighbour(shell(3), Direction::Down), None);

    // Nothing above ever answered with the shell diagonally opposite, which
    // meets the one being left at a single corner and shares no edge with it.
    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        assert_ne!(tree.neighbour(shell(0), direction), Some(shell(3)));
        assert_ne!(tree.neighbour(shell(3), direction), Some(shell(0)));
    }
}

#[test]
fn focus_moves_across_an_irregular_arrangement() {
    let tree = one_beside_two();

    // Leaving the tall column rightwards has two candidates that are equally
    // near and equally far from its middle; the topmost is chosen, and the
    // choice does not vary between runs.
    assert_eq!(tree.neighbour(shell(0), Direction::Right), Some(shell(1)));
    assert_eq!(tree.neighbour(shell(0), Direction::Right), Some(shell(1)));

    // Coming back from either row lands on the tall column, which is the only
    // thing over there at all.
    assert_eq!(tree.neighbour(shell(1), Direction::Left), Some(shell(0)));
    assert_eq!(tree.neighbour(shell(2), Direction::Left), Some(shell(0)));

    // The two rows are each other's neighbours vertically, and the tall column
    // is nobody's neighbour vertically: it is beside them, not above or below.
    assert_eq!(tree.neighbour(shell(1), Direction::Down), Some(shell(2)));
    assert_eq!(tree.neighbour(shell(2), Direction::Up), Some(shell(1)));
    assert_eq!(tree.neighbour(shell(0), Direction::Down), None);
    assert_eq!(tree.neighbour(shell(0), Direction::Up), None);
}

#[test]
fn a_shell_across_a_row_that_shares_no_edge_is_not_a_neighbour() {
    // Two rows whose dividers are nowhere near each other, so that 0 and 3 do
    // not touch at all:
    //
    // +------+-----------+
    // |  0   |     1     |
    // +------+-----------+
    // |      2     |  3  |
    // +------------+-----+
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Down, shell(2));
    tree.split(shell(0), Direction::Right, shell(1));
    tree.split(shell(2), Direction::Right, shell(3));

    // Reaching in to move the two dividers: an arrangement this lopsided is
    // ordinary to make by dragging one, and there is no other way to build it
    // out of splits, which always halve what they divide.
    let SplitTree::Split(rows) = &mut tree else {
        panic!("the tab is two rows");
    };
    assert_eq!(rows.axis(), Axis::Vertical);
    let SplitTree::Split(top) = &mut rows.children[0].tree else {
        panic!("the top row is two columns");
    };
    top.children[0].size = 0.25;
    top.children[1].size = 0.75;
    let SplitTree::Split(bottom) = &mut rows.children[1].tree else {
        panic!("the bottom row is two columns");
    };
    bottom.children[0].size = 0.75;
    bottom.children[1].size = 0.25;

    assert_eq!(
        tree.neighbour(shell(0), Direction::Down),
        Some(shell(2)),
        "0 is above part of 2 and nowhere near 3"
    );
    assert_eq!(
        tree.neighbour(shell(3), Direction::Up),
        Some(shell(1)),
        "3 is below part of 1 and nowhere near 0"
    );
    assert_eq!(
        tree.neighbour(shell(1), Direction::Down),
        Some(shell(2)),
        "the middle of 1 is over 2, though it overlaps 3 as well"
    );
    assert_eq!(
        tree.neighbour(shell(2), Direction::Up),
        Some(shell(1)),
        "the middle of 2 is under 1, though it overlaps 0 as well"
    );
}

#[test]
fn leaving_a_wide_shell_lands_on_the_one_under_its_middle() {
    // A full-width top row over three columns of different widths:
    //
    // +----------------------+
    // |          0           |
    // +----------+-----+-----+
    // |    1     |  2  |  3  |
    // +----------+-----+-----+
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Down, shell(1));
    tree.split(shell(1), Direction::Right, shell(2));
    tree.split(shell(2), Direction::Right, shell(3));

    assert_eq!(
        placed(&tree)[1],
        (1, Rect::new(0.0, 0.5, 0.5, 0.5)),
        "1 kept half the bottom row and the other two quartered the rest"
    );
    assert_eq!(
        tree.neighbour(shell(0), Direction::Down),
        Some(shell(2)),
        "the middle of the top row is over 2, not over the widest of the three"
    );
    assert_eq!(tree.neighbour(shell(1), Direction::Up), Some(shell(0)));
    assert_eq!(tree.neighbour(shell(3), Direction::Up), Some(shell(0)));
    assert_eq!(tree.neighbour(shell(2), Direction::Right), Some(shell(3)));
    assert_eq!(tree.neighbour(shell(3), Direction::Left), Some(shell(2)));
    assert_eq!(tree.neighbour(shell(1), Direction::Right), Some(shell(2)));
}

#[test]
fn asking_where_to_go_from_a_shell_that_is_not_here_answers_nowhere() {
    let tree = one_beside_two();
    assert_eq!(tree.neighbour(shell(9), Direction::Left), None);
}

#[test]
fn closing_a_shell_gives_its_space_to_a_neighbour_and_collapses_the_split() {
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Right, shell(1));

    assert_eq!(
        tree.close(shell(1)),
        Closed::Removed {
            successor: shell(0)
        }
    );
    assert_eq!(tree, SplitTree::leaf(shell(0)));
    assert_eq!(placed(&tree), vec![(0, Rect::UNIT)]);
}

#[test]
fn closing_one_of_three_along_a_row_leaves_the_row_filling_the_space() {
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Right, shell(1));
    tree.split(shell(1), Direction::Right, shell(2));

    assert_eq!(
        tree.close(shell(1)),
        Closed::Removed {
            successor: shell(0)
        }
    );

    let SplitTree::Split(split) = &tree else {
        panic!("two columns are left");
    };
    assert_eq!(split.children().len(), 2);
    let shares: f32 = split.children().iter().map(Branch::size).sum();
    assert_close(shares, 1.0, "the remaining shares");
    assert_close(split.children()[0].size(), 2.0 / 3.0, "the wide column");
    assert_close(split.children()[1].size(), 1.0 / 3.0, "the narrow column");
}

#[test]
fn the_shell_that_takes_over_is_the_one_next_to_the_one_that_went() {
    let mut tree = SplitTree::leaf(shell(0));
    tree.split(shell(0), Direction::Right, shell(1));
    tree.split(shell(1), Direction::Right, shell(2));

    assert_eq!(
        tree.close(shell(0)),
        Closed::Removed {
            successor: shell(1)
        },
        "the first column has no left-hand neighbour, so the next one takes over"
    );
    assert_eq!(
        tree.close(shell(2)),
        Closed::Removed {
            successor: shell(1)
        },
        "the last column falls back to the one before it"
    );
}

#[test]
fn a_collapsed_split_is_absorbed_rather_than_left_nested() {
    // The right-hand column holds two rows; closing one of them leaves a
    // single column-shaped thing where a column already was.
    let mut tree = one_beside_two();
    tree.split(shell(2), Direction::Right, shell(3));

    assert_eq!(
        tree.close(shell(1)),
        Closed::Removed {
            successor: shell(2)
        }
    );

    let SplitTree::Split(split) = &tree else {
        panic!("the tab is still split");
    };
    assert_eq!(split.axis(), Axis::Horizontal);
    assert_eq!(
        split.children().len(),
        3,
        "the two shells that were nested in the right-hand column joined the row"
    );
    assert_eq!(tree.shells(), vec![shell(0), shell(2), shell(3)]);
    assert_close(split.children()[0].size(), 0.5, "the untouched column");
    assert_close(
        split.children()[1].size(),
        0.25,
        "the first absorbed column",
    );
    assert_close(
        split.children()[2].size(),
        0.25,
        "the second absorbed column",
    );
    assert_eq!(
        placed(&tree),
        vec![
            (0, Rect::new(0.0, 0.0, 0.5, 1.0)),
            (2, Rect::new(0.5, 0.0, 0.25, 1.0)),
            (3, Rect::new(0.75, 0.0, 0.25, 1.0)),
        ]
    );
}

#[test]
fn closing_the_last_shell_leaves_the_tree_alone_and_says_so() {
    let mut tree = SplitTree::leaf(shell(0));
    assert_eq!(tree.close(shell(0)), Closed::Emptied);
    assert_eq!(
        tree,
        SplitTree::leaf(shell(0)),
        "a tree always holds at least one leaf, so nothing was removed"
    );
}

#[test]
fn closing_a_shell_that_is_not_here_changes_nothing() {
    let mut tree = one_beside_two();
    let before = tree.clone();
    assert_eq!(tree.close(shell(9)), Closed::NotFound);
    assert_eq!(tree, before);
}

#[test]
fn closing_every_shell_in_turn_ends_at_the_one_that_is_left() {
    let mut tree = one_beside_two();
    tree.split(shell(2), Direction::Right, shell(3));
    tree.split(shell(0), Direction::Down, shell(4));

    for going in [shell(4), shell(3), shell(1), shell(2)] {
        assert!(
            matches!(tree.close(going), Closed::Removed { .. }),
            "{going} was closed"
        );
        assert!(!tree.contains(going));
    }
    assert_eq!(tree, SplitTree::leaf(shell(0)));
    assert_eq!(tree.close(shell(0)), Closed::Emptied);
}

#[test]
fn a_direction_knows_its_axis_and_which_way_along_it_points() {
    assert_eq!(Direction::Left.axis(), Axis::Horizontal);
    assert_eq!(Direction::Right.axis(), Axis::Horizontal);
    assert_eq!(Direction::Up.axis(), Axis::Vertical);
    assert_eq!(Direction::Down.axis(), Axis::Vertical);

    assert!(Direction::Right.is_forward());
    assert!(Direction::Down.is_forward());
    assert!(!Direction::Left.is_forward());
    assert!(!Direction::Up.is_forward());

    assert_eq!(Axis::Horizontal.perpendicular(), Axis::Vertical);
    assert_eq!(Axis::Vertical.perpendicular(), Axis::Horizontal);
}
