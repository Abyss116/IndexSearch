BIN := indexsearch

.PHONY: all clean test

all: $(BIN)

$(BIN): src/main.rs src/bin/is.rs Cargo.toml
	cargo build --release
	cp target/release/indexsearch $(BIN)
	cp target/release/is is
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - $(BIN) >/dev/null 2>&1 || true; fi
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - is >/dev/null 2>&1 || true; fi

test: $(BIN)
	tests/smoke.sh

clean:
	rm -f $(BIN) is
	cargo clean
