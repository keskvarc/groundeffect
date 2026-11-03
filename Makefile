.PHONY: build check integration

build:
	cargo build --release --locked
	mkdir -p bin
	install -m755 target/release/groundeffect-engine bin/groundeffect-engine.tmp
	mv -Tf bin/groundeffect-engine.tmp bin/groundeffect-engine
	cd client && CGO_ENABLED=0 go build -trimpath -o ../bin/groundeffect.tmp .
	mv -Tf bin/groundeffect.tmp bin/groundeffect

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo test --workspace --locked
	cd client && test -z "$$(gofmt -l .)" && go vet ./... && go test -race ./...

integration: build
	python3 scripts/check_process.py
