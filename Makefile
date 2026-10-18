.PHONY: check test smoke smoke-nix

check: test
	cargo fmt --check
	SQLX_OFFLINE=true cargo clippy --locked --all-targets --all-features -- -D warnings

test:
	SQLX_OFFLINE=true cargo test --locked

# Real Telegram photo delivery against loopback, plus a credential-free caption preview.
smoke:
	SQLX_OFFLINE=true cargo test --locked --test decentrala_delivery_test --test decentrala_test
	SQLX_OFFLINE=true cargo run --locked -- decentrala-preview --date 2026-10-15 --source tests/fixtures/decentrala.csv --template templates/decentrala.j2

smoke-nix:
	bash tests/smoke_nix.sh
