BIN := indexsearch
DAEMON := is-daemon

.PHONY: all clean test

all: $(BIN)

$(BIN): src/main.rs src/frontend.rs src/bin/indexsearch.rs src/bin/is.rs Cargo.toml
	cargo build --release
	cp target/release/indexsearch $(BIN)
	cp target/release/is is
	cp target/release/is-daemon $(DAEMON)
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - $(BIN) >/dev/null 2>&1 || true; fi
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - is >/dev/null 2>&1 || true; fi
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - $(DAEMON) >/dev/null 2>&1 || true; fi

test: $(BIN)
	tests/smoke.sh

clean:
	rm -f $(BIN) is $(DAEMON)
	cargo clean
