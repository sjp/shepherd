#!/bin/sh
# Runs once, when the container is created.
#
# Three jobs: make the mounted configuration directories writable by the user who
# will actually be running the agents, install the coding agents that have no
# devcontainer feature of their own, and install the system libraries the GUI
# needs in order to link and to open a window here.

set -eu

claude_dir=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
claude_json=${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json

# The volume is root-owned on first creation, update to the container user.
mkdir -p "$claude_dir"
if [ "$(stat -c %u "$claude_dir")" != "$(id -u)" ]; then
    sudo chown -R "$(id -u):$(id -g)" "$claude_dir"
fi

# Skip onboarding and the per-folder trust dialog. Merge rather than overwrite.
claude_config=$(jq -n --arg dir "$PWD" '{
    hasCompletedOnboarding: true,
    projects: { ($dir): { hasTrustDialogAccepted: true } }
}')
if [ -f "$claude_json" ]; then
    jq --argjson add "$claude_config" '. * $add' "$claude_json" > "$claude_json.tmp"
else
    printf '%s\n' "$claude_config" > "$claude_json.tmp"
fi
mv "$claude_json.tmp" "$claude_json"

# The claude-code feature installs the package as root-owned, so
# in-place auto-updates fail with "no_permissions". Hand it to the container user.
npm_root=$(npm root -g)
if [ -d "$npm_root/@anthropic-ai" ]; then
    sudo chown -R "$(id -u):$(id -g)" "$npm_root/@anthropic-ai"
fi

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
# The last two are for driving that display rather than for having one. xauth is
# what xvfb-run needs in order to start a server at all — without it, it refuses
# rather than falling back. xrefresh is stranger and worth the sentence: the
# toolkit's X11 backend begins a window's refresh loop when it processes the
# notification that the window was mapped, and on a virtual display where nothing
# else ever happens, no further X traffic arrives to make it read that event. The
# window therefore draws its first frame and then nothing. Any traffic at all
# starts it, and one xrefresh is the cheapest way to produce some.
#
# A failure here is reported and left alone for the same reason the agent
# installs above are: the crates in this repository that exist today build and
# test without any of it.
GUI_PACKAGES="libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan1 mesa-vulkan-drivers xvfb xauth x11-xserver-utils"
if ! sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq \
	|| ! sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $GUI_PACKAGES; then
	printf '\n!! The GUI system libraries did not install. Install them by hand with\n'
	printf '!! sudo apt-get install -y %s\n' "$GUI_PACKAGES"
	printf '!! before building the GUI; the rest of this container is usable as it is.\n\n'
fi
