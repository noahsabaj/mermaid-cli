#!/bin/bash
set -e

# Mermaid CLI Installation Script
# Usage: curl -fsSL https://raw.githubusercontent.com/noahsabaj/mermaid-cli/main/install.sh | bash

RESET="\033[0m"
BOLD="\033[1m"
GREEN="\033[32m"
BLUE="\033[34m"
YELLOW="\033[33m"
RED="\033[31m"

echo -e "${BOLD}${BLUE}Mermaid CLI Installer${RESET}"
echo -e "${BLUE}=====================${RESET}\n"

# Check if running on Linux
if [[ "$OSTYPE" != "linux-gnu"* ]]; then
    echo -e "${RED}Error: This installer currently supports Linux only.${RESET}"
    exit 1
fi

# Function to check if command exists
command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Function to add cargo to PATH in current session
setup_cargo_env() {
    if [ -f "$HOME/.cargo/env" ]; then
        source "$HOME/.cargo/env"
    fi
}

echo -e "${BOLD}Step 1/4: Checking dependencies${RESET}"

# Check and install Rust
if ! command_exists cargo; then
    echo -e "${YELLOW}Rust not found. Installing Rust toolchain...${RESET}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    setup_cargo_env
    echo -e "${GREEN}Rust installed successfully!${RESET}\n"
else
    echo -e "${GREEN}Rust already installed.${RESET}\n"
fi

# Check and install Ollama
if ! command_exists ollama; then
    echo -e "${YELLOW}Ollama not found. Installing Ollama...${RESET}"
    curl -fsSL https://ollama.ai/install.sh | sh
    echo -e "${GREEN}Ollama installed successfully!${RESET}\n"
else
    echo -e "${GREEN}Ollama already installed.${RESET}\n"
fi

# Start Ollama service if not running
echo -e "${BOLD}Step 2/4: Starting Ollama service${RESET}"
if ! pgrep -x "ollama" > /dev/null; then
    echo -e "${YELLOW}Starting Ollama service...${RESET}"
    nohup ollama serve > /dev/null 2>&1 &
    sleep 2
    echo -e "${GREEN}Ollama service started.${RESET}\n"
else
    echo -e "${GREEN}Ollama service already running.${RESET}\n"
fi

# Install Mermaid
echo -e "${BOLD}Step 3/4: Installing Mermaid CLI${RESET}"
setup_cargo_env

if command_exists mermaid; then
    echo -e "${YELLOW}Mermaid already installed. Updating to latest version...${RESET}"
fi

# Install from crates.io (official releases)
echo -e "${BLUE}Installing from crates.io...${RESET}"
cargo install mermaid-cli --force

echo -e "${GREEN}Mermaid CLI installed successfully!${RESET}\n"

# Pull a compatible model
echo -e "${BOLD}Step 4/4: Downloading AI model${RESET}"
echo -e "${BLUE}Downloading llama3.1:8b (4.7GB, tool calling compatible)...${RESET}"
echo -e "${YELLOW}This may take a few minutes depending on your connection.${RESET}\n"

if ollama list | grep -q "llama3.1:8b"; then
    echo -e "${GREEN}llama3.1:8b already downloaded.${RESET}\n"
else
    ollama pull llama3.1:8b
    echo -e "${GREEN}Model downloaded successfully!${RESET}\n"
fi

# Add cargo bin to PATH permanently if not already there
SHELL_RC=""
if [ -n "$BASH_VERSION" ]; then
    SHELL_RC="$HOME/.bashrc"
elif [ -n "$ZSH_VERSION" ]; then
    SHELL_RC="$HOME/.zshrc"
fi

if [ -n "$SHELL_RC" ] && [ -f "$SHELL_RC" ]; then
    if ! grep -q 'source.*\.cargo/env' "$SHELL_RC"; then
        echo '' >> "$SHELL_RC"
        echo '# Added by Mermaid installer' >> "$SHELL_RC"
        echo 'source "$HOME/.cargo/env"' >> "$SHELL_RC"
        echo -e "${GREEN}Added cargo to PATH in $SHELL_RC${RESET}"
    fi
fi

# Success message
echo -e "\n${BOLD}${GREEN}Installation Complete!${RESET}\n"
echo -e "${BOLD}To start using Mermaid:${RESET}"
echo -e "  ${BLUE}1.${RESET} Reload your shell: ${YELLOW}source ~/.cargo/env${RESET}"
echo -e "  ${BLUE}2.${RESET} Run Mermaid: ${YELLOW}mermaid${RESET}"
echo -e ""
echo -e "${BOLD}Or run immediately (without reloading):${RESET}"
echo -e "  ${YELLOW}~/.cargo/bin/mermaid${RESET}"
echo -e ""
echo -e "${BOLD}To use a specific model:${RESET}"
echo -e "  ${YELLOW}mermaid --model llama3.1:8b${RESET}"
echo -e ""
echo -e "${BOLD}For help:${RESET}"
echo -e "  ${YELLOW}mermaid --help${RESET}"
echo -e ""
echo -e "${GREEN}Happy coding with Mermaid!${RESET}\n"
