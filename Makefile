BIN := indexsearch

.PHONY: all clean test

all: $(BIN)

$(BIN): src/main.rs Cargo.toml
	cargo build --release
	cp target/release/indexsearch $(BIN)
	@if [ "$$(uname -s)" = "Darwin" ]; then codesign --force --sign - $(BIN) >/dev/null 2>&1 || true; fi

test: $(BIN)
	tests/smoke.sh

clean:
	rm -f $(BIN)
	cargo clean
