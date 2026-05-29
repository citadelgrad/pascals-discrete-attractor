use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use attractor_types::AttractorError;

use crate::{DynProvider, ProviderAdapter, Request, Response};

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

pub trait Middleware: Send + Sync {
    fn before(&self, _request: &mut Request) {}
    fn after(&self, _request: &Request, _response: &mut Response) {}
}

// ---------------------------------------------------------------------------
// Built-in middleware: LoggingMiddleware
// ---------------------------------------------------------------------------

pub struct LoggingMiddleware;

impl Middleware for LoggingMiddleware {
    fn before(&self, request: &mut Request) {
        tracing::info!(
            model = %request.model,
            messages = request.messages.len(),
            "LLM request"
        );
    }

    fn after(&self, _request: &Request, response: &mut Response) {
        tracing::info!(
            model = %response.model,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            finish = ?response.finish_reason,
            "LLM response"
        );
    }
}

// ---------------------------------------------------------------------------
// Built-in middleware: CostTrackingMiddleware
// ---------------------------------------------------------------------------

pub struct CostTrackingMiddleware {
    total_input: Arc<AtomicU64>,
    total_output: Arc<AtomicU64>,
}

impl CostTrackingMiddleware {
    pub fn new() -> Self {
        Self {
            total_input: Arc::new(AtomicU64::new(0)),
            total_output: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn total_input_tokens(&self) -> u64 {
        self.total_input.load(Ordering::Relaxed)
    }

    pub fn total_output_tokens(&self) -> u64 {
        self.total_output.load(Ordering::Relaxed)
    }
}

impl Default for CostTrackingMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for CostTrackingMiddleware {
    fn after(&self, _request: &Request, response: &mut Response) {
        self.total_input
            .fetch_add(response.usage.input_tokens, Ordering::Relaxed);
        self.total_output
            .fetch_add(response.usage.output_tokens, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// ModelInfo / ModelCatalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub context_window: usize,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
}

pub struct ModelCatalog {
    models: HashMap<String, ModelInfo>,
}

impl ModelCatalog {
    pub fn new() -> Self {
        let mut models = HashMap::new();

        // Claude models
        for (id, ctx, reasoning) in [
            ("claude-opus-4-6", 200_000, true),
            ("claude-sonnet-4-5-20250929", 200_000, true),
            ("claude-haiku-4-5-20251001", 200_000, false),
        ] {
            models.insert(
                id.to_string(),
                ModelInfo {
                    id: id.to_string(),
                    provider: "anthropic".to_string(),
                    context_window: ctx,
                    supports_tools: true,
                    supports_reasoning: reasoning,
                },
            );
        }

        // GPT models
        for (id, ctx, reasoning) in [
            ("gpt-4o", 128_000, false),
            ("gpt-4o-mini", 128_000, false),
            ("o1", 200_000, true),
            ("o3-mini", 200_000, true),
        ] {
            models.insert(
                id.to_string(),
                ModelInfo {
                    id: id.to_string(),
                    provider: "openai".to_string(),
                    context_window: ctx,
                    supports_tools: true,
                    supports_reasoning: reasoning,
                },
            );
        }

        // Gemini models
        for (id, ctx) in [
            ("gemini-2.5-pro", 1_000_000),
            ("gemini-2.5-flash", 1_000_000),
        ] {
            models.insert(
                id.to_string(),
                ModelInfo {
                    id: id.to_string(),
                    provider: "google".to_string(),
                    context_window: ctx,
                    supports_tools: true,
                    supports_reasoning: true,
                },
            );
        }

        Self { models }
    }

    pub fn lookup(&self, model: &str) -> Option<&ModelInfo> {
        self.models.get(model)
    }

    pub fn provider_for_model(&self, model: &str) -> Option<&str> {
        self.models.get(model).map(|m| m.provider.as_str())
    }
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// LlmClient
// ---------------------------------------------------------------------------

pub struct LlmClient {
    providers: HashMap<String, DynProvider>,
    model_catalog: ModelCatalog,
    middleware: Vec<Box<dyn Middleware>>,
}

impl LlmClient {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            model_catalog: ModelCatalog::new(),
            middleware: Vec::new(),
        }
    }

    pub fn register_provider(&mut self, provider: impl ProviderAdapter + 'static) {
        let name = provider.name().to_string();
        self.providers.insert(name, DynProvider::new(provider));
    }

    pub fn with_middleware(mut self, m: impl Middleware + 'static) -> Self {
        self.middleware.push(Box::new(m));
        self
    }

    pub fn model_catalog(&self) -> &ModelCatalog {
        &self.model_catalog
    }

    pub async fn complete(&self, request: &Request) -> Result<Response, AttractorError> {
        let provider = self.resolve_provider(request)?;
        let mut req = request.clone();

        for m in &self.middleware {
            m.before(&mut req);
        }

        let mut resp = provider.complete(&req).await?;

        for m in &self.middleware {
            m.after(&req, &mut resp);
        }

        Ok(resp)
    }

    fn resolve_provider(&self, request: &Request) -> Result<&DynProvider, AttractorError> {
        // 1. Explicit provider field
        if let Some(ref provider_name) = request.provider {
            return self.providers.get(provider_name).ok_or_else(|| {
                AttractorError::Other(format!("Provider '{}' not registered", provider_name))
            });
        }

        // 2. Model catalog lookup
        if let Some(provider_name) = self.model_catalog.provider_for_model(&request.model) {
            if let Some(provider) = self.providers.get(provider_name) {
                return Ok(provider);
            }
        }

        // 3. Try each registered provider (return the first one)
        if let Some(provider) = self.providers.values().next() {
            return Ok(provider);
        }

        Err(AttractorError::Other(
            "No providers registered".to_string(),
        ))
    }

    /// Create from environment variables (detect available API keys).
    pub fn from_env() -> Result<Self, AttractorError> {
        let mut client = Self::new();
        let mut found_any = false;

        if let Ok(adapter) = crate::AnthropicAdapter::from_env() {
            client.register_provider(adapter);
            found_any = true;
        }

        if let Ok(adapter) = crate::OpenAiAdapter::from_env() {
            client.register_provider(adapter);
            found_any = true;
        }

        if let Ok(adapter) = crate::GeminiAdapter::from_env() {
            client.register_provider(adapter);
            found_any = true;
        }

        if !found_any {
            return Err(AttractorError::Other(
                "No LLM provider API keys found in environment".to_string(),
            ));
        }

        Ok(client)
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
