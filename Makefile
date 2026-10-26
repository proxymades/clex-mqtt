.PHONY: setup ui dev migrate admin check smoke ui-smoke release
setup:
	python3 scripts/local_setup.py
	npm ci
ui:
	npm run ui:build
dev: ui
	cargo run -- serve
migrate:
	cargo run -- migrate
admin:
	python3 scripts/bootstrap_admin.py
check:
	python3 scripts/check.py
smoke:
	node scripts/smoke.mjs
ui-smoke:
	node scripts/ui-smoke.mjs
release: ui
	cargo build --release --locked
