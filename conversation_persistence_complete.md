# Conversation Persistence Implementation - Complete! 🎉

## Overview
Implemented Claude Code-style conversation persistence with `--resume` and `--continue` flags, enabling users to maintain conversation history across sessions on a per-directory basis.

## Features Implemented

### 1. CLI Flags
- **`--resume`**: Shows a selection UI of all conversations in the current directory
- **`--continue`**: Automatically continues the last conversation in the current directory

### 2. In-App Commands
- **`:save [name]`**: Manually save the current conversation
- **`:load [name]`**: Load a specific conversation by ID
- **`:list`**: Show all saved conversations in this directory
- **Auto-save on exit**: Conversations are automatically saved when quitting

### 3. Storage
- Conversations are stored in `.mermaid/conversations/` within each project directory
- Each conversation is saved as a JSON file with timestamp-based ID
- Project-specific persistence - each directory maintains its own conversation history

## Architecture

### New Components
1. **ConversationHistory** (`src/session/conversation.rs`)
   - Stores complete message history with timestamps
   - Tracks model used and project path
   - Auto-generates titles from first user message

2. **ConversationManager** (`src/session/conversation.rs`)
   - Handles save/load operations
   - Lists available conversations
   - Manages `.mermaid/conversations/` directory

3. **Conversation Selector UI** (`src/session/selector.rs`)
   - Interactive TUI for selecting conversations
   - Shows title, date, message count, and model for each conversation
   - Keyboard navigation (arrow keys, j/k, Enter to select)

## How to Use

### Starting a New Session
```bash
# Regular start (new conversation)
mermaid

# With specific model
mermaid --model ollama/llama3.2:3b
```

### Resuming Previous Work
```bash
# Show selection UI to pick a conversation
mermaid --resume

# Automatically continue the last conversation
mermaid --continue
```

### In-Session Commands
```
:save          # Save current conversation
:load          # Show available conversations to load
:list          # List all conversations in this directory
:quit          # Quit and auto-save
```

## Testing Guide

### Test 1: Basic Save/Load
```bash
cd ~/test-project
mermaid
# Send: "Hello, remember the number 42"
# Type: :save
# Type: :quit
mermaid
# Type: :load
# Verify: Previous conversation appears in list
```

### Test 2: --continue Flag
```bash
cd ~/test-project
mermaid
# Have a conversation
# Quit (auto-saves)
mermaid --continue
# Verify: Previous conversation is loaded
```

### Test 3: --resume Flag
```bash
cd ~/test-project
# Create multiple conversations over time
mermaid --resume
# Verify: Selection UI appears with all conversations
# Select one with arrow keys and Enter
# Verify: Selected conversation loads
```

### Test 4: Project Isolation
```bash
cd ~/project-a
mermaid
# Have conversation A
# Quit

cd ~/project-b
mermaid
# Have conversation B
# Quit

cd ~/project-a
mermaid --resume
# Verify: Only sees conversations from project-a

cd ~/project-b
mermaid --resume
# Verify: Only sees conversations from project-b
```

## Files Changed

### New Files
- `src/session/conversation.rs` - Core conversation persistence logic
- `src/session/selector.rs` - Interactive selection UI

### Modified Files
- `src/models/types.rs` - Made ChatMessage serializable
- `src/cli/args.rs` - Added --resume and --continue flags
- `src/tui/app.rs` - Added conversation management to App
- `src/tui/ui.rs` - Added :save, :load, :list commands and auto-save
- `src/runtime/orchestrator.rs` - Integrated flag handling
- `src/session/mod.rs` - Exported new modules
- `Cargo.toml` - Added chrono serde feature

## Storage Format

Conversations are stored as JSON in `.mermaid/conversations/`:
```
project/
  .mermaid/
    conversations/
      20240320_143022.json
      20240320_151535.json
      20240321_090115.json
```

Each JSON file contains:
```json
{
  "id": "20240320_143022",
  "title": "First user message preview...",
  "messages": [...],
  "model_name": "ollama/llama3.2:3b",
  "project_path": "/home/user/project",
  "created_at": "2024-03-20T14:30:22",
  "updated_at": "2024-03-20T14:45:33",
  "total_tokens": null
}
```

## Benefits

1. **Never Lose Work**: Conversations auto-save on exit
2. **Project Context**: Each project maintains its own conversation history
3. **Quick Resume**: `--continue` instantly picks up where you left off
4. **Review Past Sessions**: Browse and load any previous conversation
5. **Share Solutions**: Conversation files can be shared with team

## Future Enhancements

1. **Export to Markdown**: Convert conversations to readable markdown
2. **Conversation Branching**: Fork from any point in a conversation
3. **Search**: Find conversations by content
4. **Merge**: Combine multiple conversations
5. **Cloud Sync**: Optional backup to cloud storage
6. **Token Tracking**: Track and display token usage per conversation

## Installation

```bash
# From the project directory
cargo install --path . --force

# Verify
mermaid --help | grep -E "resume|continue"
```

## Conclusion

The conversation persistence feature transforms Mermaid from a stateless tool into a true AI pair programmer with memory. Users can now build up project-specific knowledge over time, resume interrupted work, and never lose valuable problem-solving sessions.

This implementation follows the Claude Code pattern, making it familiar to users migrating from that tool while maintaining Mermaid's open-source, model-agnostic philosophy.