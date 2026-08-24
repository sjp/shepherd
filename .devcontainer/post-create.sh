#!/bin/sh
# Runs once, when the container is created.
#
# Three jobs: make the mounted configuration directories writable by the user who
# will actually be running the agents, install the coding agents that have no
# devcontainer feature of their own, and install the system libraries the GUI
# needs in order to link and to open a window here.

set -eu

# Each agent's configuration directory is a named volume, so it arrives owned by
# root. Every one of these agents writes its own credentials into its directory
# on first login and none of them will do it as somebody else.
sudo chown -R "$(id -u):$(id -g)" "$HOME/.claude" "$HOME/.codex" "$HOME/.config"

# Claude Code asks its onboarding questions the first time it runs, which is
# every time this container is rebuilt. This answers them once so a rebuild does
# not cost an interactive session before any work can start.
printf '%s\n' '{"hasCompletedOnboarding":true,"numStartups":1,"installMethod":"npm"}' \
	>"$HOME/.claude.json"

# Codex CLI and OpenCode have no devcontainer feature. Both publish an npm
# package, which needs nothing this container does not already have.
#
# A failure here is reported and then left alone rather than allowed to fail the
# create: an agent that did not install costs the two commands below, while a
# container that refuses to come up costs everything else in the workspace too.
# Whether they are really there is the first thing the verification checklist
# asks, and `<agent> --version` is a better answer than this script's exit code.
for package in "@openai/codex" "opencode-ai"; do
	if ! npm install --global "$package"; then
		printf '\n!! %s did not install. Install it by hand before verifying the\n' "$package"
		printf '!! agents; the rest of this container is usable as it is.\n\n'
	fi
done

# What the GUI needs from the system, and nothing else. Every entry below was
# arrived at by building and running with it absent and reading the failure, so
# the list is short on purpose: almost everything the toolkit's dependency tree
# needs it brings with it and compiles from vendored source, and the fontconfig,
# freetype and Wayland libraries a list like this usually collects are opened at
# run time by name rather than linked, so no development package is wanted for
# them.
#
# The first three are the only libraries the final link actually asks for, by
# way of -lxcb, -lxkbcommon and -lxkbcommon-x11. Notably there is no -lX11: the
# toolkit speaks xcb, so the X11 client library proper is not a dependency.
#
# The last three are what it takes to *run* rather than to build. Rendering goes
# through Vulkan, which needs a loader and at least one driver; this container
# has no /dev/dri, so the driver has to be the software rasteriser that
# mesa-vulkan-drivers carries. Without a usable driver the process does not
# degrade, it exits non-zero on startup, so these are as required as the link
# libraries are. Xvfb supplies the display a window needs to open at all.
#
# A failure here is reported and left alone for the same reason the agent
# installs above are: the crates in this repository that exist today build and
# test without any of it.
GUI_PACKAGES="libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan1 mesa-vulkan-drivers xvfb"
if ! sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq \
	|| ! sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $GUI_PACKAGES; then
	printf '\n!! The GUI system libraries did not install. Install them by hand with\n'
	printf '!! sudo apt-get install -y %s\n' "$GUI_PACKAGES"
	printf '!! before building the GUI; the rest of this container is usable as it is.\n\n'
fi
