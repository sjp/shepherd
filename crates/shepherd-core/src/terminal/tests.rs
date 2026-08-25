//! These tests start real processes on real terminal devices. Nothing here is
//! mocked, because the questions being asked — does a shell inherit the
//! variable it was given, does what it prints reach the grid, does a shell
//! nobody is looking at keep being read — are only worth asking of the thing
//! that will actually run.

use std::thread;
use std::time::{Duration, Instant};

use crate::ids::{ShellId, WorkspaceId};

use super::*;

/// How long a test waits for something a real process has to do before it
/// concludes the process is never going to do it. Generous: this has to hold on
/// a loaded machine running the rest of the suite alongside it, and the cost of
/// being generous is only paid when a test is failing anyway.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// The shell these tests run, chosen rather than inherited: every unix has it,
/// and a test that ran whatever the person running it happens to use would pass
/// and fail for reasons that have nothing to do with the code.
fn shell_options() -> ShellOptions {
    ShellOptions::new()
        .program(Program::new("/bin/sh"))
        // An empty prompt keeps the grid to the output of what was asked for.
        .env("PS1", "")
        .env("ENV", "")
}

/// The address these tests use, which is also the one they assert the
/// correlation for.
fn address() -> ShellAddress {
    ShellAddress::new(WorkspaceId::from_raw(9), ShellId::from_raw(3))
}

fn spawn(options: &ShellOptions) -> Shell {
    Shell::spawn(address(), options).expect("a shell to start")
}

/// Types a command and the return that runs it.
fn run(shell: &mut Shell, command: &str) {
    shell.write(format!("{command}\n"));
}

/// Waits until `ready`, or fails saying what it was waiting for and what was on
/// screen instead.
fn wait_for(shell: &Shell, expectation: &str, mut ready: impl FnMut(&Shell) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if ready(shell) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}; the screen said:\n{}",
            shell.screen().join("\n")
        );
        thread::sleep(GLANCE);
    }
}

/// Waits until some row of the visible screen is exactly `line`.
///
/// Exactly, rather than containing it: a command is echoed back by the terminal
/// as it is typed, so a screen showing `echo hello` proves only that the
/// keystrokes arrived. A row that is just `hello` is the process's answer.
fn wait_for_line(shell: &Shell, line: &str) {
    wait_for(shell, &format!("a row reading `{line}`"), |shell| {
        shell.screen().iter().any(|row| row == line)
    });
}

/// Waits for the process to end, and says what it ended with.
fn wait_for_exit(shell: &Shell) -> ShellState {
    wait_for(shell, "the process to end", |shell| {
        !shell.state().is_running()
    });
    shell.state()
}

#[test]
fn a_size_is_never_smaller_than_a_grid_can_be() {
    let flattened = ShellSize::new(0, 0);
    assert_eq!(flattened.columns(), MIN_COLUMNS);
    assert_eq!(flattened.lines(), MIN_LINES);

    let ordinary = ShellSize::new(120, 40);
    assert_eq!(ordinary.columns(), 120);
    assert_eq!(ordinary.lines(), 40);
    assert_eq!(Dimensions::columns(&ordinary), 120);
    assert_eq!(ordinary.screen_lines(), 40);
    assert_eq!(
        ordinary.total_lines(),
        40,
        "a size describes a screen and knows nothing about scrollback"
    );
}

#[test]
fn a_size_carries_the_pixels_a_cell_is_worth_to_the_process() {
    let measured = ShellSize::new(80, 24).with_cell(9, 21);
    assert_eq!(measured.cell_width(), 9);
    assert_eq!(measured.cell_height(), 21);

    let window: WindowSize = measured.into();
    assert_eq!(window.num_cols, 80);
    assert_eq!(window.num_lines, 24);
    assert_eq!(window.cell_width, 9);
    assert_eq!(window.cell_height, 21);

    let unmeasured: WindowSize = ShellSize::default().into();
    assert_eq!(unmeasured.cell_width, DEFAULT_CELL_WIDTH);
    assert_eq!(unmeasured.cell_height, DEFAULT_CELL_HEIGHT);
}

#[test]
fn a_shell_starts_with_its_correlation_in_the_processs_environment() {
    let mut shell = spawn(&shell_options());
    assert_eq!(shell.correlation(), "w9:s3");
    assert_eq!(shell.address(), address());

    run(&mut shell, "printf '%s\\n' \"$AGENTBUS_PANE\"");
    wait_for_line(&shell, "w9:s3");
}

#[test]
fn nothing_a_caller_puts_in_the_environment_can_pretend_to_be_another_shell() {
    let options = shell_options().env(CORRELATION_VAR, "somebody-elses-shell");
    let mut shell = spawn(&options);

    run(&mut shell, "printf '%s\\n' \"$AGENTBUS_PANE\"");
    wait_for_line(&shell, "w9:s3");
    assert!(
        !shell
            .screen()
            .iter()
            .any(|row| row.contains("somebody-elses-shell")),
        "the value the caller asked for reached the process"
    );
}

#[test]
fn a_shell_is_started_where_it_was_told_to_be() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    // Resolved because a temporary directory is often reached through a
    // symlink, and the shell reports where it really is.
    let real = directory.path().canonicalize().expect("a real path");
    let mut shell = spawn(&shell_options().directory(&real));

    run(&mut shell, "pwd");
    wait_for_line(&shell, &real.to_string_lossy());
}

#[test]
fn what_a_command_prints_turns_up_in_the_grid() {
    let mut shell = spawn(&shell_options());
    let before = shell.revision();

    run(&mut shell, "echo hello");
    wait_for_line(&shell, "hello");

    assert!(
        shell.revision() > before,
        "the grid changed without saying so"
    );
    assert!(shell.state().is_running());
}

#[test]
fn a_shell_nobody_looks_at_is_read_from_all_the_same() {
    // There is no way to tell this shell it is being watched, or to stop
    // watching it, because there is nothing here that pauses a shell. So the
    // test is that a great deal of output, produced while nothing reads the
    // grid, is all there when something finally does.
    let mut shell = spawn(&shell_options());
    run(
        &mut shell,
        "i=0; while [ $i -lt 500 ]; do i=$((i+1)); echo line$i; done; echo finished",
    );

    wait_for(&shell, "the last line of a long run", |shell| {
        shell.screen().iter().any(|row| row == "finished")
    });

    let buffer = shell.buffer();
    for line in [1, 2, 250, 499, 500] {
        let wanted = format!("line{line}");
        assert!(
            buffer.contains(&wanted),
            "`{wanted}` never reached the grid"
        );
    }
    assert!(
        buffer.len() > 500,
        "500 lines of output left a buffer of {} rows",
        buffer.len()
    );
}

#[test]
fn a_shell_remembers_far_more_than_fits_on_its_screen() {
    assert_eq!(
        DEFAULT_SCROLLBACK, 10_000,
        "the remembered amount is chosen"
    );

    let size = ShellSize::new(80, 5);
    let mut generous = spawn(&shell_options().size(size));
    let mut forgetful = spawn(&shell_options().size(size).scrollback(20));
    let count = "i=0; while [ $i -lt 200 ]; do i=$((i+1)); echo line$i; done; echo finished";

    for shell in [&mut generous, &mut forgetful] {
        run(shell, count);
    }
    for shell in [&generous, &forgetful] {
        wait_for(shell, "200 lines of output", |shell| {
            shell.screen().iter().any(|row| row == "finished")
        });
    }

    assert!(
        generous.buffer().len() > 200,
        "the default scrollback lost lines it had room for"
    );
    let kept = forgetful.buffer().len();
    assert!(
        (5..=30).contains(&kept),
        "a shell asked to remember 20 lines kept {kept} rows"
    );
}

#[test]
fn resizing_changes_the_grid_and_reflows_what_is_in_it() {
    let narrow = ShellSize::new(20, 10);
    let mut shell = spawn(&shell_options().size(narrow));
    assert_eq!(shell.size(), narrow);

    // Forty characters: two rows at twenty columns, one at eighty.
    let wide_output = "A".repeat(40);
    run(&mut shell, &format!("printf '%s\\n' {wide_output}"));
    wait_for(&shell, "output wide enough to wrap", |shell| {
        shell
            .screen()
            .iter()
            .any(|row| row.len() == 20 && row.ends_with('A'))
    });
    assert!(
        !shell.screen().contains(&wide_output),
        "forty characters fitted on a twenty-column row"
    );

    let wide = ShellSize::new(80, 10);
    shell.resize(wide);

    assert_eq!(shell.size(), wide);
    {
        let term = shell.term().lock();
        assert_eq!(term.grid().columns(), 80);
        assert_eq!(term.grid().screen_lines(), 10);
    }
    assert!(
        shell.buffer().contains(&wide_output),
        "the wrapped output did not reflow onto one row"
    );
}

#[test]
fn a_resize_reaches_the_process_as_well_as_the_grid() {
    let mut shell = spawn(&shell_options().size(ShellSize::new(20, 10)));
    run(&mut shell, "echo started");
    wait_for_line(&shell, "started");

    shell.resize(ShellSize::new(76, 21));

    // `stty` asks the terminal device, not the emulator, so an answer of the
    // right size is the process's own view of how big its terminal is.
    run(&mut shell, "stty size");
    wait_for_line(&shell, "21 76");
}

#[test]
fn a_process_that_ends_leaves_a_shell_that_says_so() {
    let mut shell = spawn(&shell_options());
    run(&mut shell, "echo going");
    wait_for_line(&shell, "going");

    run(&mut shell, "exit 7");

    let ended = wait_for_exit(&shell);
    assert!(!ended.is_running());
    assert_eq!(ended.code(), Some(7));
    assert_eq!(shell.state(), ended, "the ending was remembered");
}

#[test]
fn what_a_process_printed_survives_it_ending() {
    let mut shell = spawn(&shell_options());

    // Printed and exited in one line, so that the last output and the exit are
    // as close together as they can be.
    run(&mut shell, "echo parting; exit 0");

    assert_eq!(wait_for_exit(&shell).code(), Some(0));
    assert!(
        shell.buffer().iter().any(|row| row == "parting"),
        "the last thing printed was lost to the exit"
    );
    // Everything is still answerable, and none of it hangs.
    assert_eq!(shell.correlation(), "w9:s3");
    assert_eq!(shell.size(), ShellSize::default());
    let _ = shell.cursor();
    let _ = shell.screen();
    shell.write("this goes nowhere, and does not panic\n");
    shell.resize(ShellSize::new(40, 12));
}

#[cfg(unix)]
#[test]
fn a_shell_keeps_a_handle_on_its_terminal_device_for_as_long_as_it_exists() {
    use std::io::IsTerminal;

    let mut shell = spawn(&shell_options());
    assert!(
        shell.device().as_fd().is_terminal(),
        "the handle kept is not on a terminal"
    );

    run(&mut shell, "exit 0");
    wait_for_exit(&shell);

    // Still there afterwards: whatever asks the kernel about a shell should get
    // an answer about a shell that has ended rather than an answer about
    // whatever has since been given the same handle.
    assert!(shell.device().as_fd().is_terminal());
}

#[test]
fn the_screen_follows_the_viewport_and_the_buffer_does_not() {
    use alacritty_terminal::grid::Scroll;

    let mut shell = spawn(&shell_options().size(ShellSize::new(80, 5)));
    run(
        &mut shell,
        "i=0; while [ $i -lt 60 ]; do i=$((i+1)); echo line$i; done; echo finished",
    );
    wait_for_line(&shell, "finished");

    shell.term().lock().scroll_display(Scroll::Delta(50));

    let screen = shell.screen();
    assert!(
        !screen.contains(&"finished".to_owned()),
        "a shell scrolled back showed the bottom of its output anyway"
    );
    assert!(
        screen.iter().any(|row| row.starts_with("line")),
        "a shell scrolled back showed nothing it had scrolled back to"
    );
    assert!(
        shell.buffer().contains(&"finished".to_owned()),
        "scrolling back lost what was scrolled away from"
    );
}

#[test]
fn a_process_killed_from_outside_is_an_ending_too() {
    let mut shell = spawn(&shell_options());
    run(&mut shell, "echo alive");
    wait_for_line(&shell, "alive");

    run(&mut shell, "kill -KILL $$");

    let ended = wait_for_exit(&shell);
    assert!(!ended.is_running());
    assert_eq!(
        ended.code(),
        None,
        "a process that was killed has no exit code to report"
    );
}

#[test]
fn the_cursor_is_where_the_process_left_it() {
    let mut shell = spawn(&shell_options());
    // Printed without a newline, so the cursor stays on the same row as the
    // eight characters before it.
    run(&mut shell, "printf 'ABCDEFGH'");
    wait_for(&shell, "the cursor to move along a row", |shell| {
        shell.cursor().column == Column(8)
    });

    let cursor = shell.cursor();
    let row = &shell.screen()[usize::try_from(cursor.line.0).expect("a row on screen")];
    assert_eq!(row, "ABCDEFGH");
}
