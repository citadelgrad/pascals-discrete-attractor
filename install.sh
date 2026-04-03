#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${HOME}/.local/bin"
BIN_NAME="pas"

echo "Building Pascal's Discrete Attractor (release)..."
cargo build -p attractor-cli --release

# Create install directory if needed
mkdir -p "${INSTALL_DIR}"

# Copy binary
cp "target/release/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"

# Sign with Apple Developer identity (prevents macOS SIGKILL on unsigned binaries)
if [[ -z "${CODESIGN_IDENTITY:-}" ]]; then
    echo "ERROR: CODESIGN_IDENTITY is not set."
    echo "Set it in your .envrc (see .envrc.example) or export it before running install.sh"
    exit 1
fi
if security find-identity -v -p codesigning 2>/dev/null | grep -q "${CODESIGN_IDENTITY}"; then
    codesign --force --sign "${CODESIGN_IDENTITY}" "${INSTALL_DIR}/${BIN_NAME}"
    echo "Signed ${BIN_NAME} with ${CODESIGN_IDENTITY}"
else
    echo "WARNING: Signing identity not found, skipping codesign"
fi

echo "Installed ${BIN_NAME} to ${INSTALL_DIR}/${BIN_NAME}"

# Check if install dir is in PATH
if ! echo "${PATH}" | tr ':' '\n' | grep -qx "${INSTALL_DIR}"; then
    echo ""
    echo "WARNING: ${INSTALL_DIR} is not in your PATH."
    echo "Add it to your shell config:"
    echo ""
    echo "  # bash (~/.bashrc)"
    echo "  export PATH=\"${INSTALL_DIR}:\${PATH}\""
    echo ""
    echo "  # zsh (~/.zshrc)"
    echo "  export PATH=\"${INSTALL_DIR}:\${PATH}\""
    echo ""
    echo "  # fish (~/.config/fish/config.fish)"
    echo "  fish_add_path ${INSTALL_DIR}"
fi

echo ""
"${INSTALL_DIR}/${BIN_NAME}" --version
