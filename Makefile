# meet = the Rust TUI (crates/meet) + meet-rec = the Swift recording engine (Sources/meet).
#
# Ways to run code you just wrote (the shape is nebula's Makefile, minus what its daemon needs):
#   make dev      build and launch the latest code in an isolated instance — its own data
#                 dir, so the meetings you try land nowhere near the real ones (the first
#                 run copies your real sessions and settings in, so the session bar
#                 and the summaries look like yours; `make dev-reset` re-copies)
#   make install  release build of both binaries, symlinked into ~/.local/bin for real use
#   make cycle    install + dev in one go — the re-runnable full cutover
#
# There is no daemon and nothing to kill: the engine is a child of the TUI and stops when
# it does. What make cannot do is swap the code under a `meet` that is already running —
# it keeps the binary it started with, engine included — so quit it (q) and start it
# again; `make install` says so when it finds one. Never kill a `meet-rec` from here: the
# one recording your real meeting runs from this checkout's .build/release/meet-rec too.

ENGINE    := .build/release/meet-rec
TUI       := target/release/meet
DEBUG_TUI := target/debug/meet
PREFIX    ?= $(HOME)/.local/bin

# The dev instance is a second, complete meet: its own database, live files and settings —
# and one *per checkout*, so the main clone and every worktree can run at once, and a branch
# that migrates the schema gets a database of its own. The slot is the checkout's directory
# name plus a hash of its absolute path, so two worktrees with the same name in different
# repos still separate.
DEV_SLOT := $(shell printf '%s' '$(CURDIR)' | shasum | cut -c1-8)
DEV_DATA := $(HOME)/.meet-dev/$(notdir $(CURDIR))-$(DEV_SLOT)
# `make dev REPO=~/code/app` records in another checkout (default: this one).
REPO ?= .
# `make dev ARGS="--no-system"` passes flags through; `--replay <transcript.json>` needs no
# microphone.
ARGS ?=
# `make dev SEED=0` skips the first-run copy and starts the dev instance empty.
SEED ?= 1

# Every dev-instance run goes through this: its own data dir, and the engine this checkout
# just built rather than whichever `meet-rec` PATH finds first.
DEV_ENV = MEET_DATA_DIR=$(DEV_DATA) MEET_RECORDER=$(CURDIR)/$(ENGINE)

.DEFAULT_GOAL := help
.PHONY: help dev dev-prep dev-seed dev-reset dev-ls build engine tui install stale-note uninstall cycle test e2e run clean

help: ## Show this help
	@grep -hE '^[a-z0-9][a-z0-9-]*:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

# --- running your changes ----------------------------------------------------

dev: dev-prep ## Build and launch the latest code in an isolated instance (REPO= ARGS= SEED=0)
	@echo "dev instance [$(notdir $(CURDIR))] → data $(DEV_DATA)"
	@echo "  from another terminal: MEET_DATA_DIR=$(DEV_DATA) $(CURDIR)/$(DEBUG_TUI) ask $(REPO)"
	-@$(DEV_ENV) $(DEBUG_TUI) $(ARGS) $(REPO)

# Build and seed — everything `dev` needs before it can hand the terminal over. The engine
# is the release build either way: the Swift side is the slow, stable half, and the TUI
# only ever looks for it under .build/release.
dev-prep: engine
	cargo build
	@# On macOS the first exec of a freshly linked binary pays for signature validation and
	@# can stall for seconds. Pay it here rather than in the TUI's first frame or the engine
	@# start it is waiting on.
	@$(DEBUG_TUI) --version >/dev/null
	@$(ENGINE) --help >/dev/null
	@$(if $(filter 0,$(SEED)),true,$(MAKE) --no-print-directory dev-seed)

# A blank dev instance is useless for eyeballing a change: no session bar, nothing earlier
# for the Summary pane to show. So the first `make dev` copies the real database, live
# files and settings in, minus the meeting the real `meet` is recording right now
# (ended_at IS NULL — the dev instance must not show it as live or answer `ask` from it).
# `.backup` reads the WAL, so the copy is consistent while the real meet runs. The real dir is where
# `directories::ProjectDirs::from("dev","meet","meet")` puts it (crates/meet/src/paths.rs);
# keep the two in step.
dev-seed: ## Copy the real sessions and settings into the dev instance (only if it has none yet)
	@[ ! -e '$(DEV_DATA)/meet.db' ] || exit 0; \
	case "$$(uname -s)" in \
		Darwin) real="$$HOME/Library/Application Support/dev.meet.meet";; \
		*)      real="$${XDG_DATA_HOME:-$$HOME/.local/share}/meet";; \
	esac; \
	if [ ! -f "$$real/meet.db" ]; then \
		echo "no real meet data at $$real — dev instance starts empty"; exit 0; fi; \
	if ! command -v sqlite3 >/dev/null 2>&1; then \
		echo "sqlite3 not on PATH — dev instance starts empty"; exit 0; fi; \
	mkdir -p '$(DEV_DATA)'; \
	sqlite3 "$$real/meet.db" ".backup '$(DEV_DATA)/meet.db'"; \
	sqlite3 '$(DEV_DATA)/meet.db' "PRAGMA foreign_keys = ON; \
		DELETE FROM meetings WHERE ended_at IS NULL;"; \
	[ ! -d "$$real/sessions" ] || cp -R "$$real/sessions" '$(DEV_DATA)/'; \
	[ ! -f "$$real/settings.json" ] || cp "$$real/settings.json" '$(DEV_DATA)/'; \
	echo "seeded dev instance from $$real (sessions and settings — not the meeting being recorded now)"

dev-reset: ## Wipe this checkout's dev data; the next `make dev` re-seeds it
	rm -rf '$(DEV_DATA)'

# Slots accumulate: a worktree you deleted leaves its data behind under ~/.meet-dev. This
# lists every one, so you can `rm -rf` what you no longer need.
dev-ls: ## List every checkout's dev instance
	@for d in $(HOME)/.meet-dev/*-*/; do \
		[ -d "$$d" ] || continue; \
		printf '  %-6s %-40s %s\n' "$$(du -sh "$$d" | cut -f1)" "$$(basename $$d)" "$$d"; \
	done

# --- installing for real use -------------------------------------------------

build: engine tui ## Release build of both binaries

engine:
	swift build -c release --product meet-rec

tui:
	cargo build --release

# The symlinks point at the build outputs, so `install` swaps the code under the `meet`
# on PATH — but not under a `meet` already running, which keeps the binary it started
# with. Nothing here stops it: it may be recording a meeting.
install: build ## Symlink the release build into ~/.local/bin — says if a running meet is now stale
	mkdir -p $(PREFIX)
	ln -sf $(CURDIR)/$(ENGINE) $(PREFIX)/meet-rec
	ln -sf $(CURDIR)/$(TUI) $(PREFIX)/meet
	@echo "installed -> $(PREFIX)/meet and $(PREFIX)/meet-rec"
	@$(MAKE) --no-print-directory stale-note

stale-note:
	@pids=$$(pgrep -x meet 2>/dev/null | tr '\n' ' '); \
	[ -z "$$pids" ] || echo "note: meet is running (pid $${pids% }) with the code it started with — quit it (q) and run \`meet\` again to get this build"

uninstall: ## Remove the symlinks from ~/.local/bin
	rm -f $(PREFIX)/meet $(PREFIX)/meet-rec

# The whole cutover as one command, safe to re-run as often as you like: install first, so
# a build that fails stops here with the installed `meet` untouched; then `dev`, which
# builds the debug TUI and hands the terminal to the isolated instance. No kill step — meet
# has no daemon, and the `meet` that is running is yours to quit (it may be mid-meeting).
# Recipe lines rather than prerequisites so `make -j` cannot reorder the two.
cycle: ## Install, then run the dev instance — re-run whenever
	@$(MAKE) --no-print-directory install
	@$(MAKE) --no-print-directory dev

# --- checks ------------------------------------------------------------------

test: ## Unit tests and clippy
	cargo test
	cargo clippy --all-targets -- -D warnings

# Drive the TUI through recording, stopping, session browsing, the question session and the
# settings in tmux, against a fake claude (no API calls). Needs tmux.
e2e: tui ## The tmux end-to-end run against a fake claude
	bash crates/meet/testdata/e2e/drive.sh

# Replay the bundled demo transcript against this checkout (no microphone needed) — the
# release build, the real data dir, the real claude.
run: build ## Replay the bundled demo transcript (release build, real data)
	$(TUI) --replay crates/meet/testdata/demo-transcript.json --replay-speed 8 .

clean: ## Remove build artifacts (dev data stays; see dev-reset)
	swift package clean
	cargo clean
	rm -rf .build
