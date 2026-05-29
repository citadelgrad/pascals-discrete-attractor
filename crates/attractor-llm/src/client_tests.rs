use super::*;
use crate::{FinishReason, Message, ProviderAdapter, StreamEvent, Usage};
use async_trait::async_trait;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;

struct MockProvider {
    call_count: Arc<AtomicUsize>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl ProviderAdapter for MockProvider {
    async fn complete(&self, _request: &Request) -> Result<Response, AttractorError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        Ok(Response {
            id: "mock-resp".into(),
            text: "Hello from mock".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                total_tokens: 30,
            },
            model: "mock-model".into(),
            finish_reason: FinishReason::EndTurn,
        })
    }

    fn stream(
        &self,
        _request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(tokio_stream::empty::<StreamEvent>())
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_reasoning(&self) -> bool {
        false
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

fn make_request(model: &str, provider: Option<&str>) -> Request {
    Request {
        model: model.into(),
        messages: vec![Message::user("hello")],
        tools: vec![],
        tool_choice: None,
        max_tokens: None,
        temperature: None,
        stop_sequences: vec![],
        reasoning_effort: None,
        provider: provider.map(String::from),
        provider_options: None,
    }
}

// Test 1: register_provider and resolve
#[tokio::test]
async fn register_provider_and_complete() {
    let mut client = LlmClient::new();
    client.register_provider(MockProvider::new());

    let req = make_request("mock-model", Some("mock"));
    let resp = client.complete(&req).await.unwrap();
    assert_eq!(resp.id, "mock-resp");
    assert_eq!(resp.text, "Hello from mock");
}

// Test 2: model catalog lookup
#[test]
fn model_catalog_lookup() {
    let catalog = ModelCatalog::new();

    let info = catalog.lookup("claude-opus-4-6").unwrap();
    assert_eq!(info.provider, "anthropic");
    assert_eq!(info.context_window, 200_000);
    assert!(info.supports_tools);
    assert!(info.supports_reasoning);

    let info = catalog.lookup("gpt-4o").unwrap();
    assert_eq!(info.provider, "openai");
    assert_eq!(info.context_window, 128_000);
    assert!(!info.supports_reasoning);

    let info = catalog.lookup("gemini-2.5-pro").unwrap();
    assert_eq!(info.provider, "google");
    assert_eq!(info.context_window, 1_000_000);

    assert!(catalog.lookup("nonexistent-model").is_none());
}

// Test 3: provider resolution by model name (via catalog)
#[tokio::test]
async fn resolve_provider_by_model_name() {
    let mut client = LlmClient::new();

    // Register a provider named "anthropic" so the catalog lookup finds it
    struct AnthropicMock;

    #[async_trait]
    impl ProviderAdapter for AnthropicMock {
        async fn complete(&self, _request: &Request) -> Result<Response, AttractorError> {
            Ok(Response {
                id: "anthropic-resp".into(),
                text: "Hello from anthropic mock".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "claude-opus-4-6".into(),
                finish_reason: FinishReason::EndTurn,
            })
        }
        fn stream(
            &self,
            _request: &Request,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty::<StreamEvent>())
        }
        fn name(&self) -> &str {
            "anthropic"
        }
        fn default_model(&self) -> &str {
            "claude-opus-4-6"
        }
        fn supports_tools(&self) -> bool {
            true
        }
        fn supports_streaming(&self) -> bool {
            true
        }
        fn supports_reasoning(&self) -> bool {
            true
        }
        fn context_window_size(&self) -> usize {
            200_000
        }
    }

    client.register_provider(AnthropicMock);

    // Request with no explicit provider, but model is in catalog -> anthropic
    let req = make_request("claude-opus-4-6", None);
    let resp = client.complete(&req).await.unwrap();
    assert_eq!(resp.id, "anthropic-resp");
}

// Test 4: middleware before/after called
#[tokio::test]
async fn middleware_before_after_called() {
    let before_count = Arc::new(AtomicUsize::new(0));
    let after_count = Arc::new(AtomicUsize::new(0));

    struct CountingMiddleware {
        before_count: Arc<AtomicUsize>,
        after_count: Arc<AtomicUsize>,
    }

    impl Middleware for CountingMiddleware {
        fn before(&self, _request: &mut Request) {
            self.before_count.fetch_add(1, Ordering::Relaxed);
        }
        fn after(&self, _request: &Request, _response: &mut Response) {
            self.after_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    let bc = before_count.clone();
    let ac = after_count.clone();

    let mut client = LlmClient::new().with_middleware(CountingMiddleware {
        before_count: bc,
        after_count: ac,
    });
    client.register_provider(MockProvider::new());

    let req = make_request("mock-model", Some("mock"));
    let _resp = client.complete(&req).await.unwrap();

    assert_eq!(before_count.load(Ordering::Relaxed), 1);
    assert_eq!(after_count.load(Ordering::Relaxed), 1);
}

// Test 5: from_env with no keys returns error
#[test]
fn from_env_with_no_keys_returns_error() {
    // Ensure no provider keys are set
    std::env::remove_var("ANTHROPIC_API_KEY");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("GOOGLE_API_KEY");

    let result = LlmClient::from_env();
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.to_string().contains("No LLM provider API keys found"));
}

// Test: resolve_provider returns error when provider not registered
#[test]
fn resolve_provider_unknown_returns_error() {
    let client = LlmClient::new();
    let req = make_request("some-model", Some("nonexistent"));
    let result = client.resolve_provider(&req);
    assert!(result.is_err());
}

// Test: resolve_provider falls back to first registered provider
#[tokio::test]
async fn resolve_provider_fallback_to_first() {
    let mut client = LlmClient::new();
    client.register_provider(MockProvider::new());

    // Unknown model, no explicit provider -> fallback to first registered
    let req = make_request("unknown-model", None);
    let resp = client.complete(&req).await.unwrap();
    assert_eq!(resp.text, "Hello from mock");
}

// Test: no providers registered returns error
#[test]
fn no_providers_returns_error() {
    let client = LlmClient::new();
    let req = make_request("some-model", None);
    let result = client.resolve_provider(&req);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("No providers"));
}

// Test: CostTrackingMiddleware accumulates tokens
#[tokio::test]
async fn cost_tracking_middleware() {
    let cost = Arc::new(CostTrackingMiddleware::new());
    let cost_clone = CostTrackingMiddleware {
        total_input: cost.total_input.clone(),
        total_output: cost.total_output.clone(),
    };

    let mut client = LlmClient::new().with_middleware(cost_clone);
    client.register_provider(MockProvider::new());

    let req = make_request("mock-model", Some("mock"));
    let _resp = client.complete(&req).await.unwrap();

    assert_eq!(cost.total_input_tokens(), 10);
    assert_eq!(cost.total_output_tokens(), 20);

    // Second call accumulates
    let _resp = client.complete(&req).await.unwrap();
    assert_eq!(cost.total_input_tokens(), 20);
    assert_eq!(cost.total_output_tokens(), 40);
}

// Test: model catalog provider_for_model
#[test]
fn model_catalog_provider_for_model() {
    let catalog = ModelCatalog::new();
    assert_eq!(
        catalog.provider_for_model("claude-opus-4-6"),
        Some("anthropic")
    );
    assert_eq!(catalog.provider_for_model("gpt-4o"), Some("openai"));
    assert_eq!(
        catalog.provider_for_model("gemini-2.5-pro"),
        Some("google")
    );
    assert_eq!(catalog.provider_for_model("unknown"), None);
}
