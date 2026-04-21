// Integration tests for the shared agent loop (runtime::agent_loop)
//
// Uses a MockModel to verify the tool-call → execute → respond pipeline
// without requiring a running Ollama instance.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

use mermaid_cli::agents::{ActionResult as AgentActionResult, AgentAction};
use mermaid_cli::models::{
    ChatMessage, FunctionCall, Model, ModelCapabilities, ModelConfig, ModelResponse,
    StreamCallback, TokenUsage, ToolCall,
};
use mermaid_cli::runtime::agent_loop::{
    AgentObserver, LoopControl, MAX_AGENT_ITERATIONS, run_agent_loop,
};
use std::sync::OnceLock;

/// Shared capabilities instance for the test models. Test adapters all use
/// the same conservative defaults; sharing avoids per-model boilerplate.
fn mock_capabilities() -> &'static ModelCapabilities {
    static CAPS: OnceLock<ModelCapabilities> = OnceLock::new();
    CAPS.get_or_init(ModelCapabilities::ollama_default)
}

/// Mock model: the agent loop passes initial_tool_calls separately, then
/// calls chat() after executing those tools. This mock always returns no
/// tool calls, so the loop terminates after one iteration.
struct MockModel;

#[async_trait]
impl Model for MockModel {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _config: &ModelConfig,
        _stream_callback: Option<StreamCallback>,
    ) -> mermaid_cli::models::Result<ModelResponse> {
        // No tool calls — terminates the loop
        Ok(ModelResponse {
            content: "Done reading the file.".to_string(),
            usage: Some(TokenUsage {
                prompt_tokens: 20,
                completion_tokens: 10,
                total_tokens: 30,
            }),
            model_name: "mock".to_string(),
            thinking: None,
            tool_calls: None,
            thinking_signature: None,
        })
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> &ModelCapabilities {
        mock_capabilities()
    }

    async fn list_models(&self) -> mermaid_cli::models::Result<Vec<String>> {
        Ok(vec!["mock".to_string()])
    }
}

/// Observer that collects events for assertions
struct TestObserver {
    statuses: Vec<String>,
    tool_results: Vec<String>,
    errors: Vec<String>,
}

impl AgentObserver for TestObserver {
    fn check_interrupt(&mut self) -> LoopControl {
        LoopControl::Continue
    }

    fn on_status(&mut self, msg: &str) {
        self.statuses.push(msg.to_string());
    }

    fn on_tool_result(
        &mut self,
        tool_name: &str,
        _id: &str,
        _action: &AgentAction,
        _result: &AgentActionResult,
    ) {
        self.tool_results.push(tool_name.to_string());
    }

    fn on_error(&mut self, error: &str) {
        self.errors.push(error.to_string());
    }

    fn on_generation_start(&mut self) {}

    fn on_generation_complete(&mut self, _tokens: usize) {}
}

#[tokio::test]
async fn test_agent_loop_read_file_tool_call() {
    let model: Arc<RwLock<Box<dyn Model>>> = Arc::new(RwLock::new(Box::new(MockModel)));
    let config = ModelConfig::default();

    let mut messages = vec![
        ChatMessage::system("You are helpful."),
        ChatMessage::user("Read Cargo.toml"),
        ChatMessage::assistant("Let me read that file."),
    ];

    let initial_tool_calls = vec![ToolCall {
        id: Some("call_0".to_string()),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "Cargo.toml"}),
        },
    }];

    let mut observer = TestObserver {
        statuses: vec![],
        tool_results: vec![],
        errors: vec![],
    };

    let result = run_agent_loop(
        model,
        &config,
        &mut messages,
        initial_tool_calls,
        &mut observer,
        MAX_AGENT_ITERATIONS,
    )
    .await
    .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(!result.interrupted);
    assert_eq!(result.final_response, "Done reading the file.");
    assert_eq!(observer.tool_results, vec!["read_file"]);
    assert!(observer.errors.is_empty());
    assert!(result.total_tokens > 0);
    // The read should have succeeded (Cargo.toml exists in the project)
    assert!(!result.tool_results.is_empty());
    assert!(result.tool_results[0].success);
}

#[tokio::test]
async fn test_agent_loop_respects_max_iterations() {
    /// Model that always returns a tool call (never terminates)
    struct InfiniteToolModel;

    #[async_trait]
    impl Model for InfiniteToolModel {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _config: &ModelConfig,
            _stream_callback: Option<StreamCallback>,
        ) -> mermaid_cli::models::Result<ModelResponse> {
            Ok(ModelResponse {
                content: "Reading another file.".to_string(),
                usage: Some(TokenUsage {
                    prompt_tokens: 5,
                    completion_tokens: 5,
                    total_tokens: 10,
                }),
                model_name: "mock".to_string(),
                thinking: None,
                tool_calls: Some(vec![ToolCall {
                    id: Some("call_inf".to_string()),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "Cargo.toml"}),
                    },
                }]),
                thinking_signature: None,
            })
        }

        fn name(&self) -> &str {
            "infinite-mock"
        }

        fn capabilities(&self) -> &ModelCapabilities {
            mock_capabilities()
        }

        async fn list_models(&self) -> mermaid_cli::models::Result<Vec<String>> {
            Ok(vec![])
        }
    }

    let model: Arc<RwLock<Box<dyn Model>>> = Arc::new(RwLock::new(Box::new(InfiniteToolModel)));
    let config = ModelConfig::default();
    let mut messages = vec![ChatMessage::user("loop forever")];

    let initial_tool_calls = vec![ToolCall {
        id: Some("call_0".to_string()),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "Cargo.toml"}),
        },
    }];

    let mut observer = TestObserver {
        statuses: vec![],
        tool_results: vec![],
        errors: vec![],
    };

    let max_iters = 3;
    let result = run_agent_loop(
        model,
        &config,
        &mut messages,
        initial_tool_calls,
        &mut observer,
        max_iters,
    )
    .await
    .unwrap();

    // Loop increments iteration at the top, then checks > max_iterations.
    // So it runs max_iters iterations, then on iteration max_iters+1 it breaks.
    assert_eq!(result.iterations, max_iters + 1);
    assert!(observer.statuses.iter().any(|s| s.contains("exceeded")),);
}

#[tokio::test]
async fn test_agent_loop_interrupt() {
    struct NeverCalledModel;

    #[async_trait]
    impl Model for NeverCalledModel {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _config: &ModelConfig,
            _stream_callback: Option<StreamCallback>,
        ) -> mermaid_cli::models::Result<ModelResponse> {
            panic!("Model should not be called when interrupted before tool execution");
        }

        fn name(&self) -> &str {
            "never"
        }

        fn capabilities(&self) -> &ModelCapabilities {
            mock_capabilities()
        }

        async fn list_models(&self) -> mermaid_cli::models::Result<Vec<String>> {
            Ok(vec![])
        }
    }

    /// Observer that immediately returns Interrupt
    struct InterruptObserver;

    impl AgentObserver for InterruptObserver {
        fn check_interrupt(&mut self) -> LoopControl {
            LoopControl::Interrupt
        }
        fn on_status(&mut self, _: &str) {}
        fn on_tool_result(&mut self, _: &str, _: &str, _: &AgentAction, _: &AgentActionResult) {}
        fn on_error(&mut self, _: &str) {}
        fn on_generation_start(&mut self) {}
        fn on_generation_complete(&mut self, _: usize) {}
    }

    let model: Arc<RwLock<Box<dyn Model>>> = Arc::new(RwLock::new(Box::new(NeverCalledModel)));
    let config = ModelConfig::default();
    let mut messages = vec![ChatMessage::user("test")];

    let initial_tool_calls = vec![ToolCall {
        id: Some("call_0".to_string()),
        function: FunctionCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "Cargo.toml"}),
        },
    }];

    let mut observer = InterruptObserver;
    let result = run_agent_loop(
        model,
        &config,
        &mut messages,
        initial_tool_calls,
        &mut observer,
        MAX_AGENT_ITERATIONS,
    )
    .await
    .unwrap();

    assert!(result.interrupted);
    // iteration is incremented before check_interrupt is called
    assert_eq!(result.iterations, 1);
}

/// Ctrl+C / Esc during a long-running tool should abort the tool within
/// ~50ms and fill placeholder tool results for aborted + remaining
/// sequential tools, so message history stays consistent (every
/// `tool_call` has a matching tool response). Before the
/// `cancel_requested` polling fix, the user had to wait for all N
/// tools' internal timeouts to elapse — up to minutes for a batch of
/// `execute_command` calls.
#[tokio::test]
async fn test_agent_loop_cancels_mid_tool_and_fills_placeholders() {
    /// Observer that signals cancel on the FIRST `cancel_requested`
    /// call (during tool execution), letting the other trait methods
    /// run normally. `check_interrupt` always returns Continue so we
    /// don't trip the existing iteration-boundary interrupt path.
    struct CancelDuringToolObserver {
        cancel_delivered: bool,
    }

    impl AgentObserver for CancelDuringToolObserver {
        fn check_interrupt(&mut self) -> LoopControl {
            LoopControl::Continue
        }
        fn cancel_requested(&mut self) -> bool {
            if self.cancel_delivered {
                return false;
            }
            self.cancel_delivered = true;
            true
        }
        fn on_status(&mut self, _: &str) {}
        fn on_tool_result(&mut self, _: &str, _: &str, _: &AgentAction, _: &AgentActionResult) {}
        fn on_error(&mut self, _: &str) {}
        fn on_generation_start(&mut self) {}
        fn on_generation_complete(&mut self, _: usize) {}
    }

    /// Model that must never be called — we cancel during the first
    /// tool, which should break the loop before model.chat fires again.
    struct UnreachableModel;

    #[async_trait]
    impl Model for UnreachableModel {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _config: &ModelConfig,
            _stream_callback: Option<StreamCallback>,
        ) -> mermaid_cli::models::Result<ModelResponse> {
            panic!("model should not be called after mid-tool cancel");
        }
        fn name(&self) -> &str {
            "unreachable"
        }
        fn capabilities(&self) -> &ModelCapabilities {
            mock_capabilities()
        }
        async fn list_models(&self) -> mermaid_cli::models::Result<Vec<String>> {
            Ok(vec![])
        }
    }

    let model: Arc<RwLock<Box<dyn Model>>> = Arc::new(RwLock::new(Box::new(UnreachableModel)));
    let config = ModelConfig::default();

    let mut messages = vec![ChatMessage::user("do three things please")];

    // Three tool calls. The first is a slow execute_command (`sleep 60`)
    // that will definitely still be running when the observer signals
    // cancel on the first 50ms poll. Tools 2 and 3 never run; they
    // should land as placeholders in the history.
    let initial_tool_calls = vec![
        ToolCall {
            id: Some("call_slow".to_string()),
            function: FunctionCall {
                name: "execute_command".to_string(),
                arguments: serde_json::json!({
                    "command": "sleep 60",
                    "timeout": 120,
                }),
            },
        },
        ToolCall {
            id: Some("call_later_1".to_string()),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "Cargo.toml"}),
            },
        },
        ToolCall {
            id: Some("call_later_2".to_string()),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            },
        },
    ];

    let mut observer = CancelDuringToolObserver {
        cancel_delivered: false,
    };

    let start = std::time::Instant::now();
    let result = run_agent_loop(
        model,
        &config,
        &mut messages,
        initial_tool_calls,
        &mut observer,
        MAX_AGENT_ITERATIONS,
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();

    assert!(result.interrupted, "loop should report interrupted=true");
    assert_eq!(result.iterations, 1);
    // The cancel must take effect fast — the sleep 60 must have been
    // aborted. Cap at 5s to be generous about CI noise; in practice
    // this runs in well under a second.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cancel should abort in <5s, took {:?}",
        elapsed
    );
    // All three tool_calls got recorded as results (one real + two
    // placeholders) so the assistant message's tool_calls each have a
    // matching response.
    assert_eq!(
        result.tool_results.len(),
        3,
        "every tool_call needs a tool_result for history consistency"
    );
    // First one was the cancelled tool — also landed as a placeholder.
    assert!(!result.tool_results[0].success);
    assert!(!result.tool_results[1].success);
    assert!(!result.tool_results[2].success);
    assert!(
        result.tool_results[1].output.contains("skipped"),
        "placeholder should mention 'skipped', got {:?}",
        result.tool_results[1].output
    );
}

/// Default `cancel_requested` on a bare `AgentObserver` returns false —
/// critical for silent / subagent observers that have no user to
/// interrupt. Regression guard so future trait changes don't silently
/// flip the default.
#[test]
fn default_cancel_requested_is_false() {
    struct MinimalObserver;
    impl AgentObserver for MinimalObserver {
        fn check_interrupt(&mut self) -> LoopControl {
            LoopControl::Continue
        }
        fn on_status(&mut self, _: &str) {}
        fn on_tool_result(&mut self, _: &str, _: &str, _: &AgentAction, _: &AgentActionResult) {}
        fn on_error(&mut self, _: &str) {}
        fn on_generation_start(&mut self) {}
        fn on_generation_complete(&mut self, _: usize) {}
    }
    let mut obs = MinimalObserver;
    assert!(!obs.cancel_requested());
}
