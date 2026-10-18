.PHONY: check test smoke smoke-nix

check: test smoke
	cargo fmt --check
	SQLX_OFFLINE=true cargo clippy --locked --all-targets --all-features -- -D warnings

test:
	SQLX_OFFLINE=true cargo test --locked

# Synthetic Telegram, disposable SQLite; saved transcript and PNG under target/.
smoke:
	SQLX_OFFLINE=true cargo build --locked --example money_smoke
	python3 tests/money_smoke.py --bot "$(or $(CARGO_TARGET_DIR),target)/debug/examples/money_smoke"

smoke-nix:
	bash tests/smoke_nix.sh
