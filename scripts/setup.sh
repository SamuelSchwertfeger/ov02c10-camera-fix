#!/usr/bin/env bash
# Bootstrap a fresh Debian/Ubuntu box to build and run this project.
#
# Installs v4l-utils and cargo, and builds v4l2loopback from upstream source
# via DKMS (Debian's packaged v4l2loopback-dkms is often too old for current
# kernels, see docs/DEBUGGING.md). Device permissions and loading the module
# at boot are handled by the .deb package (`make install`).
#
# Usage:
#   ./scripts/setup.sh
set -euo pipefail

V4L2LOOPBACK_VERSION=0.15.4

echo "==> Installing system packages (apt, needs sudo)..."
sudo apt-get update
sudo apt-get install -y v4l-utils
if ! command -v cargo >/dev/null 2>&1; then
    sudo apt-get install -y cargo
fi

echo "==> Checking v4l2loopback for the running kernel ($(uname -r))..."
# Debian's packaged v4l2loopback-dkms lags upstream kernel-compat fixes (e.g.
# the setup_timer -> timer_setup API removal) and can fail to build on newer
# kernels. Build a known-good version from upstream source via DKMS instead,
# so it still auto-rebuilds on kernel upgrades.
if dkms status v4l2loopback 2>/dev/null | grep -q "$(uname -r)"; then
    echo "    v4l2loopback already built for this kernel, skipping."
elif modprobe -n v4l2loopback 2>/dev/null; then
    echo "    v4l2loopback module already available for this kernel, skipping build."
else
    sudo apt-get install -y dkms build-essential git "linux-headers-$(uname -r)"
    sudo dkms remove "v4l2loopback/${V4L2LOOPBACK_VERSION}" --all 2>/dev/null || true
    sudo apt-get remove -y v4l2loopback-dkms 2>/dev/null || true

    TMP_SRC="$(mktemp -d)"
    git clone --branch "v${V4L2LOOPBACK_VERSION}" --depth 1 \
        https://github.com/v4l2loopback/v4l2loopback.git "$TMP_SRC/v4l2loopback-${V4L2LOOPBACK_VERSION}"
    sudo rm -rf "/usr/src/v4l2loopback-${V4L2LOOPBACK_VERSION}"
    sudo cp -r "$TMP_SRC/v4l2loopback-${V4L2LOOPBACK_VERSION}" "/usr/src/v4l2loopback-${V4L2LOOPBACK_VERSION}"
    rm -rf "$TMP_SRC"

    sudo dkms add -m v4l2loopback -v "$V4L2LOOPBACK_VERSION"
    sudo dkms build -m v4l2loopback -v "$V4L2LOOPBACK_VERSION"
    sudo dkms install -m v4l2loopback -v "$V4L2LOOPBACK_VERSION"
    sudo depmod -a
fi

echo ""
echo "Setup complete. Next steps:"
echo "  make install   # build, install the package, start the camera service"
echo "  make snapshot  # or: capture one frame to check the sensor works"
echo "  make help      # see all available commands"
