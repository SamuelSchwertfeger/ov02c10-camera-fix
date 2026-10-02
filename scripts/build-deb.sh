#!/usr/bin/env bash
# Package an already-built binary as ov02c10-camera_amd64.deb.
#
# Usage:
#   scripts/build-deb.sh <binary> [version] [output dir]
#
# The package carries the binary, the systemd --user unit, and the udev /
# modprobe configuration for the virtual camera device. It does not carry
# the v4l2loopback kernel module itself (see scripts/setup.sh for that).
set -euo pipefail

BIN="${1:?usage: build-deb.sh <binary> [version] [output dir]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}"
OUT="${3:-$ROOT/target}"

PKG="$(mktemp -d)"
trap 'rm -rf "$PKG"' EXIT
chmod 755 "$PKG"

install -Dm755 "$BIN" "$PKG/usr/bin/ov02c10-camera"
install -Dm644 "$ROOT/systemd/ov02c10-camera.service" \
    "$PKG/usr/lib/systemd/user/ov02c10-camera.service"
install -Dm644 "$ROOT/LICENSE" "$PKG/usr/share/doc/ov02c10-camera/copyright"
install -d "$PKG/usr/lib/modules-load.d" "$PKG/usr/lib/modprobe.d" \
    "$PKG/usr/lib/udev/rules.d" "$PKG/DEBIAN"

# Load the virtual camera at boot, on the device number the tool expects.
# exclusive_caps=1 is what makes Chrome/Brave list it as a camera.
echo 'v4l2loopback' >"$PKG/usr/lib/modules-load.d/ov02c10-camera.conf"
echo 'options v4l2loopback video_nr=48 card_label="OV02C10 Camera" exclusive_caps=1' \
    >"$PKG/usr/lib/modprobe.d/ov02c10-camera.conf"

# The tool runs as the logged-in user and needs the capture node, the sensor
# subdevice, the media controller and the loopback device.
cat >"$PKG/usr/lib/udev/rules.d/71-ov02c10-camera.rules" <<'EOF'
SUBSYSTEM=="video4linux", GROUP="video", MODE="0660", TAG+="uaccess"
SUBSYSTEM=="media", GROUP="video", MODE="0660", TAG+="uaccess"
EOF

cat >"$PKG/DEBIAN/control" <<EOF
Package: ov02c10-camera
Version: $VERSION
Architecture: amd64
Maintainer: Samuel Schwertfeger <SamuelSchwertfeger@users.noreply.github.com>
Section: video
Priority: optional
Depends: v4l-utils
Suggests: v4l2loopback-dkms
Homepage: https://github.com/SamuelSchwertfeger/ov02c10-camera-fix
Description: Webcam bridge for OV02C10 sensors behind Intel IPU6
 Captures raw Bayer frames from the IPU6 ISYS node, debayers and exposes
 them in software, and feeds a v4l2loopback device so the camera works in
 browsers and video call applications. The sensor only runs while an
 application is using the camera.
EOF

cat >"$PKG/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    udevadm control --reload-rules 2>/dev/null || true
    modprobe v4l2loopback 2>/dev/null ||
        echo "ov02c10-camera: v4l2loopback kernel module not available yet (see the README)" >&2
    udevadm trigger --subsystem-match=video4linux --subsystem-match=media 2>/dev/null || true
    if command -v systemctl >/dev/null 2>&1; then
        # Enabled for every user at login; also (re)start it for users who
        # are logged in now so no logout is needed. Never fails the install.
        systemctl --global enable ov02c10-camera.service 2>/dev/null || true
        if command -v loginctl >/dev/null 2>&1; then
            loginctl list-users --no-legend 2>/dev/null | while read -r _ user _; do
                systemctl --user --machine="$user@.host" daemon-reload 2>/dev/null || true
                systemctl --user --machine="$user@.host" restart ov02c10-camera.service 2>/dev/null || true
            done || true
        fi
    fi
    echo "ov02c10-camera: enabled for all users; the camera sensor only turns on while an app uses it"
fi
EOF
chmod 755 "$PKG/DEBIAN/postinst"

cat >"$PKG/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] && command -v systemctl >/dev/null 2>&1; then
    if command -v loginctl >/dev/null 2>&1; then
        loginctl list-users --no-legend 2>/dev/null | while read -r _ user _; do
            systemctl --user --machine="$user@.host" stop ov02c10-camera.service 2>/dev/null || true
        done || true
    fi
    systemctl --global disable ov02c10-camera.service 2>/dev/null || true
fi
EOF
chmod 755 "$PKG/DEBIAN/prerm"

mkdir -p "$OUT"
dpkg-deb --root-owner-group --build "$PKG" "$OUT/ov02c10-camera_amd64.deb"
