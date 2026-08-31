BIN      := meet
BUILD    := .build/release/$(BIN)
PREFIX   ?= $(HOME)/.local/bin

.PHONY: build install uninstall clean run

build:
	swift build -c release

install: build
	mkdir -p $(PREFIX)
	ln -sf $(CURDIR)/$(BUILD) $(PREFIX)/$(BIN)
	@echo "installed -> $(PREFIX)/$(BIN)"

uninstall:
	rm -f $(PREFIX)/$(BIN)

clean:
	swift package clean
	rm -rf .build

run: build
	$(BUILD)
