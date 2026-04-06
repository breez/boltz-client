default: check

build:
	cargo build

build-release:
	cargo build --release

build-wasm:
	cargo build -p boltz-client --target wasm32-unknown-unknown

check: fmt-check clippy-check wasm-clippy-check test wasm-test

clippy-check:
	cargo clippy --all-targets -- -D warnings

clippy-fix:
	cargo clippy --all-targets --fix --allow-dirty --allow-staged

wasm-clippy-check:
	cargo clippy -p boltz-client --target wasm32-unknown-unknown -- -D warnings

wasm-clippy-fix:
	cargo clippy -p boltz-client --target wasm32-unknown-unknown --fix --allow-dirty --allow-staged

fmt-check:
	cargo fmt -- --check

fmt-fix:
	cargo fmt

fix: fmt-fix clippy-fix wasm-clippy-fix

test:
	cargo test

wasm-test: wasm-test-browser wasm-test-node

wasm-test-browser:
	cd crates/lib && wasm-pack test --headless --firefox -- --features browser-tests

wasm-test-node:
	cd crates/lib && wasm-pack test --node

itest:
	@cd crates/lib/regtest && ./start.sh
	@echo "Waiting for Boltz regtest stack..."; \
	for i in $$(seq 1 90); do \
		curl -sf http://localhost:9001/v2/swap/reverse > /dev/null 2>&1 \
		&& docker exec boltz-scripts bash -c "source /etc/profile.d/utils.sh && lncli-sim 1 getinfo" > /dev/null 2>&1 \
		&& break; \
		[ "$$i" = "90" ] && echo "Boltz regtest stack failed to start" && exit 1; \
		sleep 2; \
	done
	@cargo test -p boltz-client --features regtest --test regtest -- --test-threads=1; \
	rc=$$?; \
	cd crates/lib/regtest && docker compose down --volumes > /dev/null 2>&1; \
	exit $$rc
