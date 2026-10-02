SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help

CARGO ?= cargo
BIN   := target/release/ov02c10-camera
DEB   := target/ov02c10-camera_amd64.deb

.PHONY: help setup format check test build deb install uninstall clean run-loopback run-on-demand snapshot logs gain

help: ## Show targets
	@grep -E '^[a-zA-Z0-9_.-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

setup: ## Bootstrap a fresh box: v4l-utils, cargo, v4l2loopback kernel module
	./scripts/setup.sh

format: ## Format code
	$(CARGO) fmt

check: ## Format check + lint (what CI runs)
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings

test: ## Run tests
	$(CARGO) test

build: ## Build the release binary
	$(CARGO) build --release

deb: build ## Build the .deb package
	./scripts/build-deb.sh $(BIN)

install: deb ## Install the package and start the on-demand camera service
	@# Units left behind by the Python version would shadow the packaged one.
	-systemctl --user disable --now ov02c10-camera-watcher 2>/dev/null
	rm -rf ~/.config/systemd/user/ov02c10-camera.service \
		~/.config/systemd/user/ov02c10-camera.service.d \
		~/.config/systemd/user/ov02c10-camera-watcher.service
	sudo apt-get install -y --reinstall ./$(DEB)
	systemctl --user daemon-reload
	systemctl --user enable ov02c10-camera
	systemctl --user restart ov02c10-camera

uninstall: ## Stop the service and remove the package
	-systemctl --user disable --now ov02c10-camera
	sudo apt-get remove -y ov02c10-camera

clean: ## Remove build artifacts
	$(CARGO) clean

run-loopback: build ## Feed /dev/video48 continuously in the foreground (Ctrl+C to stop)
	$(BIN) --loopback -v

run-on-demand: build ## Same, but the sensor only runs while an app uses the camera
	$(BIN) --on-demand -v

snapshot: build ## Capture one frame to snapshot.ppm (quick hardware check)
	$(BIN) --snapshot snapshot.ppm -v

logs: ## Tail the camera service's logs
	journalctl --user -u ov02c10-camera -f

gain: ## Print current sensor exposure/gain control values
	v4l2-ctl -d "$$(media-ctl -d /dev/media0 -e "$$(media-ctl -d /dev/media0 -p | grep -oE 'ov02c10 [0-9]+-[0-9a-f]{4}')")" -l
