#!/bin/bash
# Start LiteLLM Proxy with Podman or Docker
# This script works with both Podman and Docker

set -e  # Exit on error

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Function to print colored messages
print_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Detect container runtime (prefer podman over docker)
if command -v podman &> /dev/null; then
    RUNTIME="podman"
    COMPOSE="podman-compose"
    print_info "Using Podman (rootless containers)"

    # Set up podman socket for rootless operation
    if [ -n "$XDG_RUNTIME_DIR" ]; then
        export DOCKER_HOST="unix://$XDG_RUNTIME_DIR/podman/podman.sock"
    fi
elif command -v docker &> /dev/null; then
    RUNTIME="docker"
    COMPOSE="docker-compose"
    print_info "Using Docker"
else
    print_error "Neither Podman nor Docker found. Please install one of them."
    echo "  - Install Podman: sudo apt-get install podman"
    echo "  - Install Docker: https://docs.docker.com/engine/install/"
    exit 1
fi

# Check if compose is installed
if ! command -v $COMPOSE &> /dev/null; then
    if [ "$RUNTIME" = "podman" ]; then
        print_error "podman-compose not found. Installing..."
        pip install --user podman-compose
        if [ $? -ne 0 ]; then
            print_error "Failed to install podman-compose"
            exit 1
        fi
    else
        print_error "docker-compose not found. Please install it."
        exit 1
    fi
fi

# Check if .env file exists, create from example if not
if [ ! -f .env ]; then
    if [ -f .env.example ]; then
        print_warning ".env file not found. Creating from .env.example..."
        cp .env.example .env
        print_info "Please edit .env file with your API keys"
    else
        print_warning ".env file not found. Creating default..."
        cat > .env << EOF
# LiteLLM Master Key (change this!)
LITELLM_MASTER_KEY=sk-mermaid-1234

# LiteLLM Proxy URL
LITELLM_PROXY_URL=http://localhost:4000

# API Keys (uncomment and add your keys)
# OPENAI_API_KEY=sk-...
# ANTHROPIC_API_KEY=sk-ant-...
# GROQ_API_KEY=gsk_...
# GOOGLE_API_KEY=...
# AZURE_API_KEY=...
EOF
        print_info "Created .env file. Add your API keys to enable providers."
    fi
fi

# Load environment variables
if [ -f .env ]; then
    export $(cat .env | grep -v '^#' | xargs)
fi

# Check if Ollama is running (for local models)
if command -v ollama &> /dev/null; then
    if ! curl -s http://localhost:11434/api/tags &> /dev/null; then
        print_warning "Ollama is not running. Starting Ollama..."
        ollama serve &> /dev/null &
        sleep 2

        # Pull a small model if no models exist
        if [ -z "$(ollama list 2>/dev/null)" ]; then
            print_info "No Ollama models found. Pulling tinyllama..."
            ollama pull tinyllama
        fi
    else
        print_info "Ollama is running"
    fi
else
    print_warning "Ollama not installed. Local models won't be available."
fi

# Parse command line arguments
COMMAND="up -d"  # Default to detached mode
FOLLOW_LOGS=false

while [[ $# -gt 0 ]]; do
    case $1 in
        -f|--follow)
            FOLLOW_LOGS=true
            shift
            ;;
        stop)
            COMMAND="down"
            shift
            ;;
        restart)
            COMMAND="restart"
            shift
            ;;
        logs)
            COMMAND="logs -f"
            shift
            ;;
        status)
            COMMAND="ps"
            shift
            ;;
        *)
            shift
            ;;
    esac
done

# Execute the compose command
print_info "Running: $COMPOSE $COMMAND"
$COMPOSE $COMMAND

# Handle post-command actions
if [[ "$COMMAND" == "up -d" ]]; then
    print_info "Waiting for LiteLLM to start..."
    sleep 5

    # Check if LiteLLM is responding
    if curl -s http://localhost:4000/health &> /dev/null; then
        print_info "✅ LiteLLM proxy is running at http://localhost:4000"
        print_info "📊 Admin UI available at http://localhost:4000/ui"

        # List available models
        print_info "Available models:"
        curl -s http://localhost:4000/models | jq -r '.data[].id' 2>/dev/null || echo "  (Install 'jq' to see model list)"

        if [ "$FOLLOW_LOGS" = true ]; then
            print_info "Following logs (Ctrl+C to stop)..."
            $COMPOSE logs -f litellm
        fi
    else
        print_error "LiteLLM is not responding. Check logs with: $COMPOSE logs litellm"
        exit 1
    fi

    # Generate systemd service for auto-start (Podman only)
    if [ "$RUNTIME" = "podman" ] && command -v systemctl &> /dev/null; then
        print_info "To enable auto-start on boot (systemd), run:"
        echo "  podman generate systemd --new --name mermaid-litellm > ~/.config/systemd/user/mermaid-litellm.service"
        echo "  systemctl --user enable mermaid-litellm.service"
    fi

elif [[ "$COMMAND" == "down" ]]; then
    print_info "LiteLLM proxy stopped"
elif [[ "$COMMAND" == "restart" ]]; then
    print_info "LiteLLM proxy restarted"
fi

# Show helpful commands
if [[ "$COMMAND" == "up -d" ]]; then
    echo ""
    print_info "Useful commands:"
    echo "  ./start_litellm.sh stop       - Stop the proxy"
    echo "  ./start_litellm.sh restart    - Restart the proxy"
    echo "  ./start_litellm.sh logs       - View logs"
    echo "  ./start_litellm.sh status     - Check status"
    echo "  ./start_litellm.sh -f         - Start and follow logs"
    echo ""
    print_info "Test with Mermaid:"
    echo "  cargo run -- --model ollama/tinyllama"
fi