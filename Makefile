BIN := indexsearch

.PHONY: all clean test

all: $(BIN)

$(BIN): src/main.rs Cargo.toml
	cargo build --release
	cp target/release/indexsearch $(BIN)

test: $(BIN)
	tests/smoke.sh

clean:
	rm -f $(BIN)
	cargo clean
