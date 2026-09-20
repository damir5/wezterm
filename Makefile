.PHONY: all fmt build check test docs servedocs

all: build

test:
	cargo nextest run
	cargo nextest run -p wezterm-escape-parser # no_std by default

check:
	cargo check
	cargo check -p wezterm-escape-parser
	cargo check -p wezterm-cell
	cargo check -p wezterm-surface
	cargo check -p wezterm-ssh

build:
	cargo build $(BUILD_OPTS) -p wezterm
	cargo build $(BUILD_OPTS) -p wezterm-gui
	cargo build $(BUILD_OPTS) -p wezterm-mux-server
	cargo build $(BUILD_OPTS) -p strip-ansi-escapes

fmt:
	cargo +nightly fmt

docs:
	ci/build-docs.sh

servedocs:
	ci/build-docs.sh serve

APP_DIR := /Applications/WezTeam.app
CACHE_DAYS ?= 14

.PHONY: install-app uninstall-app cache-size cache-gc install-cache-reminder remove-cache-reminder

install-app: ## Install /Applications/WezTeam.app — NOT needed after rebuilds; the shim picks up new debug binaries at launch. Run only when icon/shim/plist change
	mkdir -p "$(APP_DIR)/Contents/MacOS" "$(APP_DIR)/Contents/Resources"
	cp assets/wezteam/Info.plist "$(APP_DIR)/Contents/Info.plist"
	cp assets/wezteam/WezTeam.shim "$(APP_DIR)/Contents/MacOS/WezTeam"
	chmod +x "$(APP_DIR)/Contents/MacOS/WezTeam"
	cp assets/wezteam/wezteam.icns "$(APP_DIR)/Contents/Resources/terminal.icns"
	/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$(APP_DIR)"
	@echo "Installed $(APP_DIR)"

uninstall-app: ## Remove /Applications/WezTeam.app
	rm -rf "$(APP_DIR)"

cache-size: ## Show build cache size
	@du -sh $(TARGET_DIR) 2>/dev/null || true
	@du -sh $(TARGET_DIR)/debug/deps $(TARGET_DIR)/debug/incremental 2>/dev/null || true

# ponytail: find-based mtime prune; switch to cargo-sweep for fingerprint-aware cleanup
cache-gc: ## Delete build artifacts older than CACHE_DAYS days (default 14)
	find $(TARGET_DIR) -type f -mtime +$(CACHE_DAYS) -delete
	find $(TARGET_DIR) -depth -type d -empty -delete
	@du -sh $(TARGET_DIR) 2>/dev/null || true

install-cache-reminder: ## Weekly notification about build cache size (Mondays 9:47)
	mkdir -p $(HOME)/Library/LaunchAgents
	cp assets/wezteam/local.wezteam-cache-reminder.plist $(HOME)/Library/LaunchAgents/
	launchctl bootout gui/$$(id -u)/local.wezteam-cache-reminder 2>/dev/null || true
	launchctl bootstrap gui/$$(id -u) $(HOME)/Library/LaunchAgents/local.wezteam-cache-reminder.plist

remove-cache-reminder: ## Uninstall the weekly cache reminder
	launchctl bootout gui/$$(id -u)/local.wezteam-cache-reminder 2>/dev/null || true
	rm -f $(HOME)/Library/LaunchAgents/local.wezteam-cache-reminder.plist
