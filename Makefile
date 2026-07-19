.PHONY: coverage images

# Run coverage measurement locally using cargo-tarpaulin.
#
# Prerequisites:
#   cargo install cargo-tarpaulin --locked
#
# The run picks up tarpaulin.toml for all flags (engine, excludes,
# output format). Reports are written to coverage/ (Lcov only).
# Set DOCKER_HOST to a non-existent socket to skip docker-gated tests
# the same way CI does.
coverage:
	DOCKER_HOST=unix:///nonexistent cargo tarpaulin

# Build every runtime image locally with plain docker buildx.
#
# Equivalent to the old one-shot local image convenience path.
images:
	docker buildx build --platform linux/amd64 --load -t botwork/api:local -f api/Dockerfile .
	docker buildx build --platform linux/amd64 --load -t botwork/ui:local -f ui/Dockerfile .
	docker buildx build --platform linux/amd64 --load -t botwork/session-broker:local -f session-broker/Dockerfile .
	docker buildx build --platform linux/amd64 --load -t botwork/config-broker:local -f config-broker/Dockerfile .
	docker buildx build --platform linux/amd64 --load -t botwork/control-plane:local -f control-plane/Dockerfile .
	docker buildx build --platform linux/amd64 --load -t botwork/db-migrate:local -f db/migration/Dockerfile .
