# meet = the Rust TUI (crates/meet) + meet-rec = the Swift recording engine (Sources/meet).
ENGINE   := .build/release/meet-rec
TUI      := target/release/meet
PREFIX   ?= $(HOME)/.local/bin

.PHONY: build engine tui install uninstall clean test e2e run

build: engine tui

engine:
	swift build -c release --product meet-rec

tui:
	cargo build --release

install: build
	mkdir -p $(PREFIX)
	ln -sf $(CURDIR)/$(ENGINE) $(PREFIX)/meet-rec
	ln -sf $(CURDIR)/$(TUI) $(PREFIX)/meet
	@echo "installed -> $(PREFIX)/meet and $(PREFIX)/meet-rec"

uninstall:
	rm -f $(PREFIX)/meet $(PREFIX)/meet-rec

test:
	cargo test
	cargo clippy --all-targets -- -D warnings

# Drive the TUI through stop / resume / quit-and-resume / session browsing in tmux, against a
# fake claude and a fake gh (no API calls, no real pull requests). Needs tmux.
e2e: tui
	bash crates/meet/testdata/e2e/drive.sh

clean:
	swift package clean
	cargo clean
	rm -rf .build

# Replay the bundled demo transcript against this checkout (no microphone needed).
run: build
	$(TUI) --replay crates/meet/testdata/demo-transcript.json --replay-speed 8 .
