#!/bin/sh
# Runs once, when the container is created.
#
# Two jobs: make the mounted configuration directories writable by the user who
# will actually be running the agents, and install the coding agents that have no
# devcontainer feature of their own.

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
