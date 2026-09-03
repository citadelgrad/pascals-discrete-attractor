use async_trait::async_trait;
use futures_core::Stream;
use std::pin::Pin;

use crate::{Request, Response, StreamEvent};

// ---------------------------------------------------------------------------
// ProviderAdapter
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn complete(
        &self,
        request: &Request,
    ) -> Result<Response, attractor_types::AttractorError>;
    fn stream(&self, request: &Request) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>>;
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;
    fn supports_tools(&self) -> bool;
    fn supports_streaming(&self) -> bool;
    fn supports_reasoning(&self) -> bool;
    fn context_window_size(&self) -> usize;
}

// ---------------------------------------------------------------------------
// DynProvider
// ---------------------------------------------------------------------------

pub struct DynProvider(Box<dyn ProviderAdapter>);

impl DynProvider {
    pub fn new(provider: impl ProviderAdapter + 'static) -> Self {
        Self(Box::new(provider))
    }

    pub async fn complete(
        &self,
        request: &Request,
    ) -> Result<Response, attractor_types::AttractorError> {
        self.0.complete(request).await
    }

    pub fn stream(
        &self,
        request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        self.0.stream(request)
    }

    pub fn name(&self) -> &str {
        self.0.name()
    }

    pub fn default_model(&self) -> &str {
        self.0.default_model()
    }

    pub fn supports_tools(&self) -> bool {
        self.0.supports_tools()
    }

    pub fn supports_streaming(&self) -> bool {
        self.0.supports_streaming()
    }

    pub fn supports_reasoning(&self) -> bool {
        self.0.supports_reasoning()
    }

    pub fn context_window_size(&self) -> usize {
        self.0.context_window_size()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FinishReason, Message, Usage};
    use std::collections::HashMap;
    use tokio_stream::StreamExt;

    struct MockProvider;

    #[async_trait]
    impl ProviderAdapter for MockProvider {
        async fn complete(
            &self,
            _request: &Request,
        ) -> Result<Response, attractor_types::AttractorError> {
            Ok(Response {
                id: "mock-resp-1".into(),
                text: "Hello from mock".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::EndTurn,
            })
        }

        fn stream(
            &self,
            _request: &Request,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::once(StreamEvent::ContentStart))
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

    fn make_test_request() -> Request {
        Request {
            model: "mock-model".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            reasoning_effort: None,
            provider: None,
            provider_options: None,
        }
    }

    #[tokio::test]
    async fn dyn_provider_complete() {
        let provider = DynProvider::new(MockProvider);
        let req = make_test_request();
        let resp = provider.complete(&req).await.unwrap();
        assert_eq!(resp.id, "mock-resp-1");
        assert_eq!(resp.text, "Hello from mock");
        assert_eq!(resp.finish_reason, FinishReason::EndTurn);
    }

    #[test]
    fn dyn_provider_capability_methods() {
        let provider = DynProvider::new(MockProvider);
        assert_eq!(provider.name(), "mock");
        assert_eq!(provider.default_model(), "mock-model");
        assert!(provider.supports_tools());
        assert!(provider.supports_streaming());
        assert!(!provider.supports_reasoning());
        assert_eq!(provider.context_window_size(), 128_000);
    }

    #[tokio::test]
    async fn streaming_capability_fixture_yields_a_real_event() {
        let provider = DynProvider::new(MockProvider);
        assert!(provider.supports_streaming());

        let request = make_test_request();
        let event = provider.stream(&request).next().await;
        assert!(matches!(event, Some(StreamEvent::ContentStart)));
    }

    #[tokio::test]
    async fn builtin_provider_streaming_claims_match_their_empty_streams() {
        let providers = [
            DynProvider::new(crate::AnthropicAdapter::new("test-key".into())),
            DynProvider::new(crate::OpenAiAdapter::new("test-key".into())),
            DynProvider::new(crate::GeminiAdapter::new("test-key".into())),
        ];
        let request = make_test_request();

        for provider in providers {
            assert!(
                !provider.supports_streaming(),
                "{} must not claim unsupported streaming",
                provider.name()
            );
            assert!(
                provider.stream(&request).next().await.is_none(),
                "{} unexpectedly emitted a streaming event",
                provider.name()
            );
        }
    }

    #[tokio::test]
    async fn dyn_provider_in_hashmap() {
        let mut providers: HashMap<String, DynProvider> = HashMap::new();
        providers.insert("mock".into(), DynProvider::new(MockProvider));

        let provider = providers.get("mock").unwrap();
        assert_eq!(provider.name(), "mock");

        let req = make_test_request();
        let resp = provider.complete(&req).await.unwrap();
        assert_eq!(resp.text, "Hello from mock");
    }
}
