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

#[test]
fn an_arrangement_built_whole_is_the_one_splitting_would_have_produced() {
    let mut grown = SplitTree::leaf(shell(0));
    grown.split(shell(0), Direction::Right, shell(1));
    grown.split(shell(1), Direction::Down, shell(2));

    let built = SplitTree::split_of(
        Axis::Horizontal,
        vec![
            Branch::new(0.5, SplitTree::leaf(shell(0))),
            Branch::new(
                0.5,
                SplitTree::split_of(
                    Axis::Vertical,
                    vec![
                        Branch::new(0.5, SplitTree::leaf(shell(1))),
                        Branch::new(0.5, SplitTree::leaf(shell(2))),
                    ],
                )
                .expect("a column of two"),
            ),
        ],
    )
    .expect("a row of two, the second a column");

    assert_eq!(built, grown);
}

#[test]
fn an_arrangement_that_breaks_an_invariant_is_refused_rather_than_built() {
    let column = || {
        SplitTree::split_of(
            Axis::Vertical,
            vec![
                Branch::new(0.5, SplitTree::leaf(shell(1))),
                Branch::new(0.5, SplitTree::leaf(shell(2))),
            ],
        )
        .expect("a column of two")
    };

    assert_eq!(
        SplitTree::split_of(
            Axis::Vertical,
            vec![Branch::new(1.0, SplitTree::leaf(shell(0)))]
        ),
        Err(MalformedSplit::TooFewChildren(1))
    );
    assert_eq!(
        SplitTree::split_of(Axis::Vertical, Vec::new()),
        Err(MalformedSplit::TooFewChildren(0))
    );
    assert_eq!(
        SplitTree::split_of(
            Axis::Vertical,
            vec![
                Branch::new(0.5, SplitTree::leaf(shell(0))),
                Branch::new(0.5, column()),
            ]
        ),
        Err(MalformedSplit::NestedAxis(Axis::Vertical)),
        "a column inside a column is a column"
    );
    assert_eq!(
        SplitTree::split_of(
            Axis::Horizontal,
            vec![
                Branch::new(0.5, SplitTree::leaf(shell(1))),
                Branch::new(0.5, column()),
            ]
        ),
        Err(MalformedSplit::DuplicateShell(shell(1))),
        "a shell cannot be in two places at once"
    );
    for share in [0.0, -0.5, f32::NAN, f32::INFINITY] {
        assert!(
            matches!(
                SplitTree::split_of(
                    Axis::Horizontal,
                    vec![
                        Branch::new(share, SplitTree::leaf(shell(0))),
                        Branch::new(0.5, SplitTree::leaf(shell(1))),
                    ]
                ),
                Err(MalformedSplit::Share(_))
            ),
            "{share} was taken as a share of a split"
        );
    }
}

#[test]
fn shares_are_rescaled_only_when_they_do_not_already_divide_the_space() {
    let built = SplitTree::split_of(
        Axis::Horizontal,
        vec![
            Branch::new(3.0, SplitTree::leaf(shell(0))),
            Branch::new(1.0, SplitTree::leaf(shell(1))),
        ],
    )
    .expect("a row of two");
    assert_eq!(
        placed(&built),
        vec![
            (0, Rect::new(0.0, 0.0, 0.75, 1.0)),
            (1, Rect::new(0.75, 0.0, 0.25, 1.0)),
        ]
    );

    // A tree that has been closed down to size has shares that sum to one to
    // within rounding, and rebuilding it must not disturb them.
    let mut grown = SplitTree::leaf(shell(0));
    for added in 1..5 {
        grown.split(shell(added - 1), Direction::Right, shell(added));
    }
    grown.close(shell(2));
    let SplitTree::Split(row) = &grown else {
        panic!("a row of four");
    };
    let rebuilt = SplitTree::split_of(row.axis(), row.children().to_vec()).expect("the same row");
    assert_eq!(rebuilt, grown);
}

/// The divider between two of a split's children, found by where it lies rather
/// than by counting: a test that said "the second one" would still pass if the
/// walk changed its mind about the order, and would then be asserting something
/// else.
fn divider_at(tree: &SplitTree, axis: Axis, along: f32) -> PlacedDivider {
    let dividers = tree.dividers();
    let found: Vec<_> = dividers
        .iter()
        .filter(|placed| placed.axis == axis && (placed.bounds.start(axis) - along).abs() < 1e-5)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one divider on {axis} at {along}, and this arrangement has {dividers:#?}"
    );
    found[0].clone()
}

#[test]
fn a_tree_of_one_shell_has_nothing_between_anything() {
    assert!(SplitTree::leaf(shell(0)).dividers().is_empty());
}

#[test]
fn a_divider_lies_on_the_edge_it_separates_and_spans_its_split() {
    let tree = one_beside_two();
    let dividers = tree.dividers();
    assert_eq!(
        dividers.len(),
        2,
        "a row of two, one of them a column of two"
    );

    let between_columns = divider_at(&tree, Axis::Horizontal, 0.5);
    assert_eq!(
        between_columns.bounds,
        Rect::new(0.5, 0.0, 0.0, 1.0),
        "the divider between the columns runs the whole height of the tab"
    );
    assert_eq!(
        between_columns.within,
        Rect::UNIT,
        "and a position for it is a fraction of the tab"
    );

    let between_rows = divider_at(&tree, Axis::Vertical, 0.5);
    assert_eq!(
        between_rows.bounds,
        Rect::new(0.5, 0.5, 0.5, 0.0),
        "the one between the rows crosses only the column holding them"
    );
    assert_eq!(
        between_rows.within,
        Rect::new(0.5, 0.0, 0.5, 1.0),
        "and a position for it is a fraction of that column"
    );
}

#[test]
fn a_divider_is_laid_out_at_the_scale_it_was_asked_for() {
    let tree = one_beside_two();
    let within = Rect::new(0.0, 0.0, 800.0, 600.0);
    let dividers = tree.dividers_in(within);

    let columns = dividers
        .iter()
        .find(|placed| placed.axis == Axis::Horizontal)
        .expect("the divider between the columns");
    assert_eq!(columns.bounds, Rect::new(400.0, 0.0, 0.0, 600.0));
    assert_eq!(columns.within, within);
}

#[test]
fn dragging_a_divider_moves_the_edge_it_was_taken_from() {
    let mut tree = one_beside_two();
    let between_columns = divider_at(&tree, Axis::Horizontal, 0.5);

    assert!(tree.resize(&between_columns.divider, 0.75));

    assert_eq!(
        placed(&tree),
        vec![
            (0, Rect::new(0.0, 0.0, 0.75, 1.0)),
            (1, Rect::new(0.75, 0.0, 0.25, 0.5)),
            (2, Rect::new(0.75, 0.5, 0.25, 0.5)),
        ],
        "the left column grew and the column of two shrank to fit"
    );
    assert!(
        !tree.resize(&between_columns.divider, 0.75),
        "putting it back where it already is changes nothing"
    );
}

#[test]
fn a_divider_inside_a_split_is_a_fraction_of_that_split() {
    let mut tree = one_beside_two();
    let between_rows = divider_at(&tree, Axis::Vertical, 0.5);

    // A quarter of the way down the tab, which is a quarter of the way down the
    // column too, because that column is the full height of the tab.
    assert!(tree.resize(&between_rows.divider, 0.25));

    assert_eq!(
        placed(&tree),
        vec![
            (0, Rect::new(0.0, 0.0, 0.5, 1.0)),
            (1, Rect::new(0.5, 0.0, 0.5, 0.25)),
            (2, Rect::new(0.5, 0.25, 0.5, 0.75)),
        ],
        "only the two rows sharing that edge moved"
    );
}

#[test]
fn a_divider_moves_the_two_shells_it_separates_and_nothing_else() {
    // A row of three, each a third of it.
    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Right, shell(1)));
    assert!(tree.split(shell(1), Direction::Right, shell(2)));
    let third = placed(&tree)[2].1;

    let first = divider_at(&tree, Axis::Horizontal, 0.5);
    assert!(tree.resize(&first.divider, 0.1));

    let after = placed(&tree);
    assert_close(after[0].1.width, 0.1, "the first shell");
    assert_close(after[1].1.width, 0.65, "the second shell");
    assert_eq!(after[2].1, third, "the third shell did not move");
}

#[test]
fn a_divider_stops_where_its_neighbours_have_nothing_left_to_give() {
    let mut tree = SplitTree::leaf(shell(0));
    assert!(tree.split(shell(0), Direction::Right, shell(1)));
    let between = divider_at(&tree, Axis::Horizontal, 0.5);

    assert!(tree.resize(&between.divider, -3.0));
    let squeezed = placed(&tree);
    assert_close(squeezed[0].1.width, MINIMUM_SHARE, "the shell dragged over");
    assert!(
        squeezed[0].1.width > 0.0,
        "a shell dragged to nothing could never be dragged back"
    );

    assert!(tree.resize(&between.divider, 40.0));
    let squeezed = placed(&tree);
    assert_close(
        squeezed[1].1.width,
        MINIMUM_SHARE,
        "the shell dragged the other way",
    );

    assert!(
        !tree.resize(&between.divider, f32::NAN),
        "a position that is not a number is not a position"
    );
}

#[test]
fn a_divider_the_arrangement_does_not_have_moves_nothing() {
    let mut tree = one_beside_two();
    let before = tree.clone();
    let inside = divider_at(&tree, Axis::Vertical, 0.5).divider;

    // The arrangement the divider was taken from is gone: closing the shell
    // below collapsed the column it was a boundary of.
    assert!(matches!(tree.close(shell(2)), Closed::Removed { .. }));
    assert!(!tree.resize(&inside, 0.25));

    let mut alone = SplitTree::leaf(shell(0));
    assert!(
        !alone.resize(&inside, 0.25),
        "a tree with no dividers at all"
    );
    assert_ne!(tree, before, "the closing did happen");
}
