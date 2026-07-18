.PHONY: coverage

# Run coverage measurement locally using cargo-tarpaulin.
#
# Prerequisites:
#   cargo install cargo-tarpaulin --locked
#
# The run picks up tarpaulin.toml for all flags (engine, excludes,
# output format). Reports are written to coverage/ (Lcov + Xml).
# Set DOCKER_HOST to a non-existent socket to skip docker-gated tests
# the same way CI does.
coverage:
	DOCKER_HOST=unix:///nonexistent cargo tarpaulin
