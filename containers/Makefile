SHELL := /bin/bash

.DEFAULT_GOAL := containers

VERSION := $(shell [ -f ../VERSION ] && tr -d '[:space:]' < ../VERSION || echo 0.0.0-dev)
GHCR_PREFIX := ghcr.io/botworkz/botwork
SERVICES := session-broker
EARTHLY ?= earthly

.PHONY: containers tarballs clean $(SERVICES)

define tag_image
	@if [ -n "$(VERSION)" ]; then \
		docker tag botwork/$(1):local $(GHCR_PREFIX)/$(1):$(VERSION); \
		docker tag botwork/$(1):local $(GHCR_PREFIX)/$(1):latest; \
	fi
endef

containers: $(SERVICES)

session-broker:
	cd .. && $(EARTHLY) +$@-image
	$(call tag_image,$@)

tarballs: containers
	mkdir -p dist
	@for svc in $(SERVICES); do \
		docker save botwork/$$svc:local -o dist/$$svc.tar; \
	done

clean:
	rm -rf dist
